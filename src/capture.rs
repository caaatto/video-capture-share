use crate::frame::{Frame, SharedFrame};
use anyhow::{Context, Result, anyhow};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType, Resolution,
};
use nokhwa::{Camera, query};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest {
    pub device_index: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
}

pub fn list_devices() -> Result<()> {
    let devices = query(ApiBackend::Auto).context("failed to enumerate capture devices")?;
    if devices.is_empty() {
        println!("No capture devices found.");
        println!();
        println!("On Windows: check Device Manager for the capture card under Cameras or Imaging devices.");
        return Ok(());
    }
    println!("Capture devices:");
    for d in devices {
        println!("  [{}] {}   {}", d.index(), d.human_name(), d.description());
    }
    println!();
    println!("Open one with: --device <INDEX>");
    Ok(())
}

pub fn spawn(req: CaptureRequest, sink: SharedFrame) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = run(req, sink) {
                log::error!("capture thread exited: {e:#}");
            }
        })?;
    Ok(handle)
}

fn run(req: CaptureRequest, sink: SharedFrame) -> Result<()> {
    let format = pick_format(&req);
    let index = CameraIndex::Index(req.device_index);
    let mut cam = Camera::new(index, format)
        .with_context(|| format!("failed to open device {}", req.device_index))?;

    cam.open_stream().context("failed to start capture stream")?;
    let res = cam.resolution();
    log::info!(
        "capture opened: {}x{} @ {} fps, format {:?}",
        res.width(),
        res.height(),
        cam.frame_rate(),
        cam.frame_format()
    );

    let mut seq: u64 = 0;
    loop {
        let buf = match cam.frame() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("dropped frame: {e}");
                continue;
            }
        };
        let decoded = match buf.decode_image::<RgbFormat>() {
            Ok(d) => d,
            Err(e) => {
                log::warn!("decode failed: {e}");
                continue;
            }
        };
        let (w, h) = (decoded.width(), decoded.height());
        let rgb = decoded.into_raw();
        if rgb.len() != (w as usize) * (h as usize) * 3 {
            return Err(anyhow!("unexpected frame size: {} bytes for {}x{}", rgb.len(), w, h));
        }
        seq = seq.wrapping_add(1);
        sink.publish(Frame { width: w, height: h, rgb: Arc::new(rgb), seq });
    }
}

fn pick_format(req: &CaptureRequest) -> RequestedFormat<'static> {
    match (req.width, req.height, req.fps) {
        (Some(w), Some(h), Some(fps)) => RequestedFormat::new::<RgbFormat>(
            RequestedFormatType::Closest(nokhwa::utils::CameraFormat::new(
                Resolution::new(w, h),
                nokhwa::utils::FrameFormat::MJPEG,
                fps,
            )),
        ),
        (Some(w), Some(h), None) => RequestedFormat::new::<RgbFormat>(
            RequestedFormatType::HighestResolution(Resolution::new(w, h)),
        ),
        (_, _, Some(fps)) => RequestedFormat::new::<RgbFormat>(
            RequestedFormatType::HighestFrameRate(fps),
        ),
        _ => RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
    }
}
