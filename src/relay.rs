use crate::frame::SharedFrame;
use crate::settings::Settings;
use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{ColorType, ImageEncoder};
use parking_lot::Mutex;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tiny_http::{Header, Method, Response, Server};

const BOUNDARY: &str = "vcshareframe";

pub fn spawn(
    addr: SocketAddr,
    shared: SharedFrame,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    let server = Server::http(addr)
        .map_err(|e| anyhow::anyhow!("failed to bind {addr}: {e}"))?;
    std::thread::Builder::new()
        .name("relay-accept".into())
        .spawn(move || accept_loop(server, shared, settings))
        .context("failed to spawn relay accept thread")?;
    Ok(())
}

fn accept_loop(server: Server, shared: SharedFrame, settings: Arc<Mutex<Settings>>) {
    for request in server.incoming_requests() {
        let shared = shared.clone();
        let settings = settings.clone();
        let url = request.url().to_string();
        let method = request.method().clone();
        std::thread::Builder::new()
            .name(format!("relay-{}", request.remote_addr().map(|a| a.to_string()).unwrap_or_default()))
            .spawn(move || {
                if let Err(e) = handle(request, &url, &method, shared, settings) {
                    log::debug!("client gone: {e}");
                }
            })
            .ok();
    }
}

fn handle(
    request: tiny_http::Request,
    url: &str,
    method: &Method,
    shared: SharedFrame,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    if method != &Method::Get {
        let _ = request.respond(Response::from_string("method not allowed").with_status_code(405));
        return Ok(());
    }
    match url {
        "/" | "/index.html" => serve_index(request),
        "/stream" | "/stream.mjpg" => serve_mjpeg(request, shared, settings),
        "/snapshot.jpg" => serve_snapshot(request, shared, settings),
        _ => {
            let _ = request.respond(Response::from_string("not found").with_status_code(404));
            Ok(())
        }
    }
}

fn serve_index(request: tiny_http::Request) -> Result<()> {
    let html = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>video capture share</title>
<style>
  html,body{margin:0;background:#000;height:100%}
  img{display:block;width:100%;height:100%;object-fit:contain}
</style></head>
<body><img src="/stream" alt="capture"></body></html>"#;
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    request.respond(Response::from_string(html).with_header(header))?;
    Ok(())
}

fn serve_snapshot(
    request: tiny_http::Request,
    shared: SharedFrame,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    let Some(frame) = shared.get() else {
        let _ = request.respond(Response::from_string("no frame yet").with_status_code(503));
        return Ok(());
    };
    let quality = settings.lock().jpeg_quality.clamp(1, 100);
    let jpeg = encode_jpeg(&frame.rgb, frame.width, frame.height, quality)?;
    let header = Header::from_bytes(&b"Content-Type"[..], &b"image/jpeg"[..]).unwrap();
    request.respond(Response::from_data(jpeg).with_header(header))?;
    Ok(())
}

fn serve_mjpeg(
    request: tiny_http::Request,
    shared: SharedFrame,
    settings: Arc<Mutex<Settings>>,
) -> Result<()> {
    let mut writer = request.into_writer();
    write!(writer, "HTTP/1.1 200 OK\r\n")?;
    write!(writer, "Content-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\n")?;
    write!(writer, "Cache-Control: no-store, no-cache, must-revalidate, max-age=0\r\n")?;
    write!(writer, "Pragma: no-cache\r\n")?;
    write!(writer, "Connection: close\r\n")?;
    write!(writer, "\r\n")?;
    writer.flush()?;

    let mut last_seq: u64 = 0;
    loop {
        let frame = match shared.get() {
            Some(f) if f.seq != last_seq => f,
            _ => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
        };
        last_seq = frame.seq;
        let quality = settings.lock().jpeg_quality.clamp(1, 100);
        let jpeg = encode_jpeg(&frame.rgb, frame.width, frame.height, quality)?;
        write!(
            writer,
            "--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            jpeg.len()
        )?;
        writer.write_all(&jpeg)?;
        writer.write_all(b"\r\n")?;
        writer.flush()?;
    }
}

fn encode_jpeg(rgb: &[u8], w: u32, h: u32, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(w as usize * h as usize / 4);
    let encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder.write_image(rgb, w, h, ColorType::Rgb8.into())?;
    Ok(out)
}
