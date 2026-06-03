use crate::frame::{Frame, SharedFrame, UiEvent};
use anyhow::{Context, Result, anyhow, bail};
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, CameraIndex, CameraInfo, FrameFormat, RequestedFormat,
    RequestedFormatType, Resolution,
};
use nokhwa::{Camera, query};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use winit::event_loop::EventLoopProxy;

#[derive(Debug, Clone, Copy)]
pub struct CaptureRequest {
    pub device_index: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub force_mjpeg: bool,
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
        CameraIndex::String(_) => 0,
    }
}

/// Open the device, list every supported (resolution, format, fps) it reports,
/// and exit. Useful for diagnosing capture cards that lie about their modes.
pub fn probe(device_index: u32) -> Result<()> {
    let placeholder = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
    let mut cam = Camera::new(CameraIndex::Index(device_index), placeholder)
        .with_context(|| format!("failed to open device {device_index} for probing"))?;
    let mut formats = cam
        .compatible_camera_formats()
        .context("failed to list compatible formats")?;
    formats.sort_by(format_priority);
    println!("Device {device_index} supports {} modes:", formats.len());
    for f in &formats {
        println!(
            "  {}x{} @ {} fps  {:?}",
            f.resolution().width(),
            f.resolution().height(),
            f.frame_rate(),
            f.format()
        );
    }
    Ok(())
}

pub fn spawn(
    req: CaptureRequest,
    sink: SharedFrame,
    notifier: Option<EventLoopProxy<UiEvent>>,
) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("capture".into())
        .spawn(move || {
            if let Err(e) = run(req, sink, notifier) {
                log::error!("capture thread exited: {e:#}");
            }
        })?;
    Ok(handle)
}

fn run(
    req: CaptureRequest,
    sink: SharedFrame,
    notifier: Option<EventLoopProxy<UiEvent>>,
) -> Result<()> {
    // Open once. MSMF is finicky about rapid open/close, so we keep the same
    // Camera handle for the format query AND for streaming. Probing then
    // closing then reopening triggers "Hardware MFT failed to start streaming
    // due to lack of hardware resources" on most cheap cards.
    let placeholder = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
    let index = CameraIndex::Index(req.device_index);
    let mut cam = Camera::new(index, placeholder)
        .with_context(|| format!("failed to open device {}", req.device_index))?;

    let formats = cam
        .compatible_camera_formats()
        .context("failed to list compatible formats")?;
    let chosen = pick_best_format_from(&formats, &req)?;
    log::info!(
        "selected mode: {}x{} @ {} fps  {:?}",
        chosen.resolution().width(),
        chosen.resolution().height(),
        chosen.frame_rate(),
        chosen.format()
    );

    cam.set_camera_requset(RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(chosen)))
        .context("failed to apply chosen capture format")?;
    cam.open_stream().context("failed to start capture stream")?;

    let mut seq: u64 = 0;
    let mut decode_fails: u64 = 0;
    let mut last_warn = Instant::now();
    let mut frames_since_log: u64 = 0;
    let mut last_log = Instant::now();
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
                decode_fails += 1;
                if last_warn.elapsed() >= Duration::from_secs(5) {
                    log::warn!(
                        "{decode_fails} decode failures in the last interval, last: {e}"
                    );
                    last_warn = Instant::now();
                    decode_fails = 0;
                }
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
        if let Some(n) = notifier.as_ref() {
            // If the loop is gone (window closed), the proxy fails; that just
            // means we're about to exit anyway.
            let _ = n.send_event(UiEvent::FrameReady);
        }

        frames_since_log += 1;
        let elapsed = last_log.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let fps = frames_since_log as f64 / elapsed.as_secs_f64();
            log::info!("capture {:.1} fps", fps);
            last_log = Instant::now();
            frames_since_log = 0;
        }
    }
}

/// Picks the best (resolution, format, fps) for this device. Done by opening
/// briefly, listing every mode the device reports, scoring and sorting them.
///
/// Heuristic, in order of priority:
///  1. Honor explicit width, height, fps from CLI flags as hard targets.
///  2. Prefer raw formats (YUYV, NV12, RGB) over MJPEG. Cheap capture cards
///     often emit broken MJPEG, and even good MJPEG costs CPU to decode.
///  3. Prefer >= 30 fps. Reject 1 fps degenerate modes some cards advertise.
///  4. Prefer the highest resolution <= 1920x1080. Above that is rare on these
///     cards and the user is usually upscaling anyway.
fn pick_best_format_from(formats: &[CameraFormat], req: &CaptureRequest) -> Result<CameraFormat> {
    if formats.is_empty() {
        bail!("device reported zero capture formats");
    }
    let candidates: Vec<&CameraFormat> = formats
        .iter()
        .filter(|f| matches_request(f, req))
        .collect();
    let chosen = candidates
        .iter()
        .copied()
        .min_by(|a, b| format_priority(a, b))
        .or_else(|| formats.iter().min_by(|a, b| format_priority(a, b)))
        .ok_or_else(|| anyhow!("could not pick a capture format"))?;
    Ok(*chosen)
}

fn matches_request(f: &CameraFormat, req: &CaptureRequest) -> bool {
    if let Some(w) = req.width {
        if f.resolution().width() != w {
            return false;
        }
    }
    if let Some(h) = req.height {
        if f.resolution().height() != h {
            return false;
        }
    }
    if let Some(fps) = req.fps {
        if f.frame_rate() != fps {
            return false;
        }
    }
    if !req.force_mjpeg && f.frame_rate() < 5 {
        return false;
    }
    true
}

/// Lower score wins. Strict total order: pixel format, then smoothness class,
/// then fps descending, then closeness to 1080p.
fn format_priority(a: &CameraFormat, b: &CameraFormat) -> std::cmp::Ordering {
    pixel_priority(a.format())
        .cmp(&pixel_priority(b.format()))
        .then_with(|| smooth_class(a.frame_rate()).cmp(&smooth_class(b.frame_rate())))
        .then_with(|| b.frame_rate().cmp(&a.frame_rate()))
        .then_with(|| res_distance(a, 1920, 1080).cmp(&res_distance(b, 1920, 1080)))
}

fn smooth_class(fps: u32) -> u8 {
    if fps >= 30 { 0 } else { 1 }
}

fn pixel_priority(fmt: FrameFormat) -> u8 {
    match fmt {
        FrameFormat::YUYV => 0,
        FrameFormat::NV12 => 1,
        FrameFormat::RAWRGB | FrameFormat::RAWBGR => 2,
        FrameFormat::GRAY => 3,
        FrameFormat::MJPEG => 4,
    }
}

fn res_distance(f: &CameraFormat, pref_w: u32, pref_h: u32) -> u64 {
    let w = f.resolution().width();
    let h = f.resolution().height();
    let over_w = w.saturating_sub(pref_w) as u64 * 2;
    let over_h = h.saturating_sub(pref_h) as u64 * 2;
    let under_w = pref_w.saturating_sub(w) as u64;
    let under_h = pref_h.saturating_sub(h) as u64;
    over_w + over_h + under_w + under_h
}

// Suppress unused-import warning if Resolution becomes unused after edits.
#[allow(dead_code)]
fn _keep_resolution_used(_: Resolution) {}
