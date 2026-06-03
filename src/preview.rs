use crate::frame::{Frame, SharedFrame};
use anyhow::{Context, Result};
use softbuffer::{Context as SbContext, Surface};
use std::num::NonZeroU32;
use std::rc::Rc;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

pub fn run(shared: SharedFrame) -> Result<()> {
    let event_loop = EventLoop::new().context("failed to create event loop")?;
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App { shared, win: None, last_seq: 0, last_log: Instant::now(), frames_since_log: 0 };
    event_loop.run_app(&mut app).context("event loop exited with error")?;
    Ok(())
}

struct WinState {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    shared: SharedFrame,
    win: Option<WinState>,
    last_seq: u64,
    last_log: Instant,
    frames_since_log: u64,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.win.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("video capture share")
            .with_inner_size(PhysicalSize::new(960u32, 540u32));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                log::error!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };
        let ctx = match SbContext::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                log::error!("softbuffer context failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = match Surface::new(&ctx, window.clone()) {
            Ok(s) => s,
            Err(e) => {
                log::error!("softbuffer surface failed: {e}");
                event_loop.exit();
                return;
            }
        };
        self.win = Some(WinState { window, surface });
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.win.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Some(frame) = self.shared.get() {
                    if frame.seq != self.last_seq {
                        self.last_seq = frame.seq;
                        draw_frame(state, &frame);
                        self.frames_since_log += 1;
                    }
                }
                tick_fps_log(&mut self.last_log, &mut self.frames_since_log);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.win.as_ref() {
            state.window.request_redraw();
        }
        // Sleep a hair so we do not spin the CPU when no frame is ready. The
        // capture thread is what gates real frame timing.
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn draw_frame(state: &mut WinState, frame: &Frame) {
    let size = state.window.inner_size();
    let (w, h) = (size.width.max(1), size.height.max(1));
    let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) else { return };
    if let Err(e) = state.surface.resize(nw, nh) {
        log::warn!("surface resize failed: {e}");
        return;
    }
    let mut buf = match state.surface.buffer_mut() {
        Ok(b) => b,
        Err(e) => {
            log::warn!("surface buffer failed: {e}");
            return;
        }
    };
    scale_rgb_to_xrgb(&frame.rgb, frame.width, frame.height, &mut buf, w, h);
    if let Err(e) = buf.present() {
        log::warn!("buffer present failed: {e}");
    }
}

/// Nearest neighbor scale from packed RGB8 source to 0x00RRGGBB destination.
/// Chosen because it is cheap and predictable. Quality is fine for preview.
fn scale_rgb_to_xrgb(src: &[u8], sw: u32, sh: u32, dst: &mut [u32], dw: u32, dh: u32) {
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return;
    }
    let sw_i = sw as usize;
    let sh_i = sh as usize;
    // Fixed point step in 16.16 to avoid per-pixel floating point.
    let x_step = ((sw_i << 16) / dw as usize) as u32;
    let y_step = ((sh_i << 16) / dh as usize) as u32;
    let mut sy_fp: u32 = 0;
    for y in 0..dh as usize {
        let sy = (sy_fp >> 16) as usize;
        let row_start = sy * sw_i * 3;
        let row = &src[row_start..row_start + sw_i * 3];
        let out_row = &mut dst[y * dw as usize..(y + 1) * dw as usize];
        let mut sx_fp: u32 = 0;
        for px in out_row.iter_mut() {
            let sx = (sx_fp >> 16) as usize;
            let i = sx * 3;
            let r = row[i] as u32;
            let g = row[i + 1] as u32;
            let b = row[i + 2] as u32;
            *px = (r << 16) | (g << 8) | b;
            sx_fp = sx_fp.wrapping_add(x_step);
        }
        sy_fp = sy_fp.wrapping_add(y_step);
    }
}

fn tick_fps_log(last: &mut Instant, frames: &mut u64) {
    let now = Instant::now();
    let elapsed = now.duration_since(*last);
    if elapsed >= Duration::from_secs(5) {
        let fps = *frames as f64 / elapsed.as_secs_f64();
        log::info!("preview {:.1} fps", fps);
        *last = now;
        *frames = 0;
    }
}
