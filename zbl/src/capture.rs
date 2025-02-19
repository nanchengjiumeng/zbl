use std::sync::mpsc::{sync_channel, Receiver, TryRecvError, TrySendError};

use windows::{
    core::{IInspectable, Interface, Result},
    Foundation::TypedEventHandler,
    Graphics::{
        Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX, D3D11_MAPPED_SUBRESOURCE,
        D3D11_TEXTURE2D_DESC,
    },
};

use crate::{
    staging_texture::StagingTexture,
    util::{create_d3d_device, create_direct3d_device, get_dxgi_interface_from_object},
    Capturable,
};

pub struct Frame<'a> {
    pub texture: &'a StagingTexture,
    pub ptr: D3D11_MAPPED_SUBRESOURCE,
}

pub struct Capture {
    device: ID3D11Device,
    direct3d_device: IDirect3DDevice,
    context: ID3D11DeviceContext,
    capturable: Box<dyn Capturable>,
    capture_box: D3D11_BOX,
    capture_done_signal: Receiver<()>,
    frame_pool: Direct3D11CaptureFramePool,
    frame_source: Receiver<Option<Direct3D11CaptureFrame>>,
    session: GraphicsCaptureSession,
    staging_texture: Option<StagingTexture>,
    content_size: SizeInt32,
    stopped: bool,
}

impl Capture {
    /// Create a new capture. This will initialize D3D11 devices, context, and Windows.Graphics.Capture's
    /// frame pool / capture session.
    ///
    /// Note that this will not start capturing yet. Call `start()` to actually start receiving frames.
    pub fn new(capturable: Box<dyn Capturable>, capture_cursor: bool) -> Result<Self> {
        let device = create_d3d_device()?;
        let context = unsafe {
            let mut d3d_context = None;
            device.GetImmediateContext(&mut d3d_context);
            d3d_context.expect("failed to create d3d_context")
        };
        let direct3d_device = create_direct3d_device(&device)?;

        let capture_item = capturable.create_capture_item()?;
        let capture_item_size = capture_item.Size()?;

        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            capture_item_size,
        )?;

        let session = frame_pool.CreateCaptureSession(&capture_item)?;
        session.SetIsCursorCaptureEnabled(capture_cursor)?;

        let (sender, receiver) = sync_channel(1 << 5);
        frame_pool.FrameArrived(
            &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |frame_pool, _| {
                    let frame_pool = frame_pool.as_ref().unwrap();
                    let frame = frame_pool.TryGetNextFrame()?;
                    let ts = frame.SystemRelativeTime()?;
                    match sender.try_send(Some(frame)) {
                        Err(TrySendError::Full(_)) => {
                            // TODO keep track of these frames?
                            println!("dropping frame {}", ts.Duration);
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            println!("frame receiver disconnected");
                        }
                        _ => {}
                    }
                    Ok(())
                },
            ),
        )?;

        let capture_box = capturable.get_client_box()?;
        let capture_done_signal = capturable.get_close_notification_channel();

        Ok(Self {
            device,
            direct3d_device,
            context,
            capturable,
            capture_box,
            capture_done_signal,
            frame_pool,
            frame_source: receiver,
            session,
            staging_texture: None,
            content_size: Default::default(),
            stopped: false,
        })
    }

    /// Get attached capturable.
    pub fn capturable(&self) -> &Box<dyn Capturable> {
        &self.capturable
    }

    /// Start capturing frames.
    pub fn start(&self) -> Result<()> {
        self.session.StartCapture()
    }

    /// Grab current capture frame.
    ///
    /// **This method blocks if there is no frames in the frame pool** (happens when application's window
    /// is minimized, for example).
    ///
    /// Returns:
    /// * `Ok(Some(...))` if there is a frame and it's been successfully captured;
    /// * `Ok(None)` if no frames can be received (e.g. when the window was closed).
    /// * `Err(...)` if an error has occured while capturing a frame.
    pub fn grab(&mut self) -> Result<Option<Frame>> {
        if self.grab_next()? {
            let texture = self.staging_texture.as_ref().unwrap();
            let ptr = self
                .staging_texture
                .as_ref()
                .unwrap()
                .as_mapped(&self.context)?;
            Ok(Some(Frame { texture, ptr }))
        } else {
            Ok(None)
        }
    }

    /// Stops the capture.
    ///
    /// This `Capture` instance cannot be reused after that (i.e. calling `start()` again will
    /// **not** produce more frames).
    pub fn stop(&mut self) -> Result<()> {
        self.stopped = true;
        self.session.Close()?;
        self.frame_pool.Close()?;
        Ok(())
    }

    fn recreate_frame_pool(&mut self) -> Result<()> {
        let capture_item = self.capturable.create_capture_item()?;
        let capture_item_size = capture_item.Size()?;
        self.capture_box = self.capturable.get_client_box()?;
        self.frame_pool.Recreate(
            &self.direct3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            capture_item_size,
        )?;
        Ok(())
    }

    fn grab_next(&mut self) -> Result<bool> {
        if self.stopped {
            return Ok(false);
        }
        let frame = loop {
            match self.frame_source.try_recv() {
                Ok(Some(f)) => break f,
                Err(TryRecvError::Empty) => {
                    // TODO busy loop? so uncivilized
                    if let Ok(()) | Err(TryRecvError::Disconnected) =
                        self.capture_done_signal.try_recv()
                    {
                        self.stop()?;
                        return Ok(false);
                    }
                }
                Ok(None) | Err(TryRecvError::Disconnected) => return Ok(false),
            }
        };

        let frame_texture: ID3D11Texture2D = get_dxgi_interface_from_object(&frame.Surface()?)?;
        let content_size = frame.ContentSize()?;

        if self.content_size.Width != content_size.Width
            || self.content_size.Height != content_size.Height
            || self.staging_texture.is_none()
        {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { frame_texture.GetDesc(&mut desc) };
            self.recreate_frame_pool()?;
            let new_staging_texture = StagingTexture::new(
                &self.device,
                self.capture_box.right - self.capture_box.left,
                self.capture_box.bottom - self.capture_box.top,
                desc.Format,
            )?;
            self.staging_texture = Some(new_staging_texture);
            self.content_size = content_size;
        }

        let copy_dest = self.staging_texture.as_ref().unwrap().as_resource()?;
        let copy_src = frame_texture.cast()?;
        unsafe {
            self.context.CopySubresourceRegion(
                Some(&copy_dest),
                0,
                0,
                0,
                0,
                Some(&copy_src),
                0,
                Some(&self.capture_box as *const _),
            );
        }

        // TODO queue a fence here? currently we ensure buffer is copied by map-unmap texture outside of this method,
        // which is probably not the best way to do this

        Ok(true)
    }
}
