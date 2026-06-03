use anyhow::{Context, Result};
use clap::Parser;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;

mod capture;
mod frame;
mod preview;
mod relay;
mod settings;

#[derive(Parser, Debug)]
#[command(
    name = "video-capture-share",
    version,
    about = "Low overhead capture card preview and LAN relay",
    long_about = None,
)]
struct Cli {
    /// Index of the capture device to open. Omit to pick interactively.
    #[arg(short, long)]
    device: Option<u32>,

    /// Print the list of devices and exit. Useful for scripts.
    #[arg(long)]
    list: bool,

    /// Open the device, print every supported mode it reports, then exit.
    #[arg(long)]
    probe: bool,

    /// Accept MJPEG and other low fps modes if nothing better fits. By default
    /// modes below 5 fps are rejected because cheap cards advertise them and
    /// they are useless.
    #[arg(long)]
    allow_mjpeg: bool,

    /// Requested width. The device picks the closest supported mode.
    #[arg(long)]
    width: Option<u32>,

    /// Requested height.
    #[arg(long)]
    height: Option<u32>,

    /// Requested frames per second.
    #[arg(long)]
    fps: Option<u32>,

    /// Bind address for the MJPEG HTTP relay, e.g. 0.0.0.0:8080. Omit to skip.
    #[arg(long)]
    serve: Option<SocketAddr>,

    /// JPEG quality for the relay, 1 to 100. Live-adjustable from the F1 panel.
    #[arg(long, default_value_t = 75)]
    quality: u8,

    /// Run without opening a preview window. Useful when only relaying.
    #[arg(long)]
    headless: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    if cli.list {
        let devices = capture::enumerate()?;
        capture::print_devices(&devices);
        return Ok(());
    }

    let device_index = match cli.device {
        Some(i) => i,
        None => capture::pick_device_interactive()?,
    };

    if cli.probe {
        return capture::probe(device_index);
    }

    let request = capture::CaptureRequest {
        device_index,
        width: cli.width,
        height: cli.height,
        fps: cli.fps,
        force_mjpeg: cli.allow_mjpeg,
    };

    let shared_settings = Arc::new(Mutex::new(settings::Settings {
        jpeg_quality: cli.quality,
        ..settings::Settings::default()
    }));

    let capture_info = settings::CaptureInfo {
        fps_target: cli.fps.unwrap_or(60),
        format_label: if cli.allow_mjpeg { "any".into() } else { "raw preferred".into() },
    };

    let shared = frame::SharedFrame::new();

    // Build the event loop up front so we can pass a proxy to the capture
    // thread. The capture thread wakes the loop on each new frame, which is
    // what keeps the GPU and CPU idle when nothing is changing.
    let (event_loop, notifier) = if cli.headless {
        (None, None)
    } else {
        let el = preview::build_event_loop()?;
        let proxy = el.create_proxy();
        (Some(el), Some(proxy))
    };

    let _capture_handle = capture::spawn(request, shared.clone(), notifier)
        .context("failed to start capture thread")?;

    if let Some(addr) = cli.serve {
        relay::spawn(addr, shared.clone(), shared_settings.clone())
            .context("failed to start MJPEG relay")?;
        log::info!("serving MJPEG at http://{}/", addr);
    }

    match event_loop {
        None => {
            log::info!("headless mode, no preview window. Press Ctrl C to exit.");
            loop {
                std::thread::park();
            }
        }
        Some(el) => preview::run(el, shared, shared_settings, capture_info)?,
    }

    Ok(())
}
