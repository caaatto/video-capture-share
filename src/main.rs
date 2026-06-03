use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;

mod capture;
mod frame;
mod preview;
mod relay;

#[derive(Parser, Debug)]
#[command(
    name = "video-capture-share",
    version,
    about = "Low overhead capture card preview and LAN relay",
    long_about = None,
)]
struct Cli {
    /// Index of the capture device to open. Omit to list devices and exit.
    #[arg(short, long)]
    device: Option<u32>,

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

    /// JPEG quality for the relay, 1 to 100. Lower means less CPU and bandwidth.
    #[arg(long, default_value_t = 75)]
    quality: u8,

    /// Run without opening a preview window. Useful when only relaying.
    #[arg(long)]
    headless: bool,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    let Some(device_index) = cli.device else {
        return capture::list_devices();
    };

    let request = capture::CaptureRequest {
        device_index,
        width: cli.width,
        height: cli.height,
        fps: cli.fps,
    };

    let shared = frame::SharedFrame::new();
    let _capture_handle = capture::spawn(request, shared.clone())
        .context("failed to start capture thread")?;

    if let Some(addr) = cli.serve {
        relay::spawn(addr, shared.clone(), cli.quality)
            .context("failed to start MJPEG relay")?;
        log::info!("serving MJPEG at http://{}/", addr);
    }

    if cli.headless {
        log::info!("headless mode, no preview window. Press Ctrl C to exit.");
        loop {
            std::thread::park();
        }
    } else {
        preview::run(shared)?;
    }

    Ok(())
}
