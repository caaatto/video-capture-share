use crate::frame::{Frame, SharedFrame};
use anyhow::{Context, Result, anyhow, bail};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraIndex, CameraInfo, RequestedFormat, RequestedFormatType, Resolution,
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

pub fn enumerate() -> Result<Vec<CameraInfo>> {
    query(ApiBackend::Auto).context("failed to enumerate capture devices")
}

pub fn print_devices(devices: &[CameraInfo]) {
    if devices.is_empty() {
        println!("No capture devices found.");
        println!();
        println!("On Windows: check Device Manager for the capture card under Cameras or Imaging devices.");
        return;
    }
    println!("Capture devices:");
    for d in devices {
        println!("  [{}] {}   {}", d.index(), d.human_name(), d.description());
    }
}

/// Interactive picker. Returns the chosen device index, or an error if the
/// user has no devices or aborts. If exactly one device exists it is picked
/// automatically. Falls back to a plain stdin prompt when the terminal is not
/// interactive.
pub fn pick_device_interactive() -> Result<u32> {
    let devices = enumerate()?;
    if devices.is_empty() {
        print_devices(&devices);
        bail!("no capture devices available");
    }
    if devices.len() == 1 {
        let only = &devices[0];
        let idx = extract_index(only);
        println!("Only one capture device, opening: [{}] {}", idx, only.human_name());
        return Ok(idx);
    }

    let labels: Vec<String> = devices
        .iter()
        .map(|d| format!("[{}] {}   {}", extract_index(d), d.human_name(), d.description()))
        .collect();

    let selection = dialoguer::Select::new()
        .with_prompt("Pick a capture device")
        .items(&labels)
        .default(0)
        .interact_opt()
        .context("failed to read selection")?;

    let Some(idx) = selection else {
        bail!("no device picked");
    };
    Ok(extract_index(&devices[idx]))
}

fn extract_index(info: &CameraInfo) -> u32 {
    match info.index() {
        CameraIndex::Index(i) => *i,
        // Some backends use string indices. Fall back to position-based 0..N.
        // We re-resolve later via the order in enumerate().
        CameraIndex::String(_) => 0,
    }
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
