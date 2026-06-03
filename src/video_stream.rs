// H.264 + MPEG-TS pipeline.
//
// Spawned alongside the relay when the platform supports it (Windows for
// now, since the H.264 encoder is Media Foundation). Pulls NV12 frames from
// the shared capture slot, runs them through the MSMF H.264 encoder, wraps
// the resulting NAL units in MPEG-TS packets and broadcasts the bytes to
// every connected `/stream.ts` client without blocking the encoder.

#![cfg(windows)]

use crate::frame::{FrameData, SharedFrame};
use crate::h264_encoder::H264Encoder;
use crate::mpegts::MpegTsMuxer;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::{Duration, Instant};

/// Fan-out for MPEG-TS byte chunks. Encoder broadcasts; clients subscribe
/// and read from their own bounded channel. A slow client just gets its
/// chunks dropped, the encoder never blocks.
pub struct TsBroadcaster {
    subscribers: Mutex<Vec<SyncSender<Arc<Vec<u8>>>>>,
}

impl TsBroadcaster {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { subscribers: Mutex::new(Vec::new()) })
    }

    pub fn subscribe(&self) -> Receiver<Arc<Vec<u8>>> {
        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(64);
        self.subscribers.lock().push(tx);
        rx
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len()
    }

    fn broadcast(&self, chunk: Arc<Vec<u8>>) {
        let mut subs = self.subscribers.lock();
        subs.retain(|tx| tx.try_send(chunk.clone()).is_ok());
    }
}

/// Spawn the encoder + muxer thread. It stays up as long as `shutdown`
/// stays false. Encoder is built lazily once we know the input dimensions.
pub fn spawn(
    shared: SharedFrame,
    broadcaster: Arc<TsBroadcaster>,
    shutdown: Arc<AtomicBool>,
    bitrate_bps: u32,
) {
    let _ = std::thread::Builder::new()
        .name("relay-h264".into())
        .spawn(move || pipeline(shared, broadcaster, shutdown, bitrate_bps));
}

fn pipeline(
    shared: SharedFrame,
    broadcaster: Arc<TsBroadcaster>,
    shutdown: Arc<AtomicBool>,
    bitrate_bps: u32,
) {
    let mut encoder: Option<H264Encoder> = None;
    let mut muxer = MpegTsMuxer::new();
    let mut current_dims: (u32, u32) = (0, 0);
    let mut last_seq: u64 = 0;
    let start = Instant::now();
    let mut last_fps_log = Instant::now();
    let mut encoded_frames: u64 = 0;
    // Cached SPS + PPS in Annex-B form. Captured from the first sample that
    // contains them and re-prepended to every subsequent keyframe AU so
    // clients that join mid-stream can decode the next IDR.
    let mut sps_pps: Vec<u8> = Vec::new();

    while !shutdown.load(Ordering::Relaxed) {
        let frame = match shared.get() {
            Some(f) if f.seq != last_seq => f,
            _ => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
        };
        last_seq = frame.seq;
        let nv12 = match &frame.data {
            FrameData::Nv12(b) => b.clone(),
            // The H.264 encoder requires NV12 input. If the device gave us
            // RGB instead (e.g. MJPEG / YUYV source), we cannot stream it.
            FrameData::Rgb(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        // (Re)build the encoder if this is the first frame or dimensions changed.
        if encoder.is_none() || current_dims != (frame.width, frame.height) {
            match H264Encoder::new(frame.width, frame.height, 60, bitrate_bps) {
                Ok(enc) => {
                    encoder = Some(enc);
                    current_dims = (frame.width, frame.height);
                    muxer = MpegTsMuxer::new();
                    log::info!(
                        "H.264 pipeline ready: {}x{} -> MPEG-TS",
                        frame.width, frame.height
                    );
                }
                Err(e) => {
                    log::error!("H.264 encoder init failed: {e:#}");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }
        }
        let enc = encoder.as_mut().expect("created above");

        let pts_d = frame.captured_at.saturating_duration_since(start);
        let pts_90khz = MpegTsMuxer::duration_to_90khz(pts_d);
        // IMFSample wants 100ns ticks.
        let pts_100ns = (pts_d.as_nanos() / 100) as i64;

        let nals = match enc.encode(nv12.as_ref(), pts_100ns) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("H.264 encode error: {e:#}");
                continue;
            }
        };

        for nal_blob in nals {
            let types: Vec<u8> = iter_nals(&nal_blob).map(|(_, t)| t).collect();
            if encoded_frames % 60 == 0 {
                log::info!("frame #{} NAL types: {:?}", encoded_frames, types);
            }
            if sps_pps.is_empty() {
                if let Some(extracted) = extract_sps_pps(&nal_blob) {
                    log::info!("captured SPS+PPS ({} bytes) for IDR re-injection", extracted.len());
                    sps_pps = extracted;
                }
            }

            let keyframe = annexb_has_idr(&nal_blob);
            let has_inline_sps = annexb_contains_nal_type(&nal_blob, 7);
            let payload: Vec<u8> = if keyframe && !sps_pps.is_empty() && !has_inline_sps {
                log::info!(
                    "prepending {} bytes of SPS+PPS to IDR AU",
                    sps_pps.len()
                );
                let mut combined = Vec::with_capacity(sps_pps.len() + nal_blob.len());
                combined.extend_from_slice(&sps_pps);
                combined.extend_from_slice(&nal_blob);
                combined
            } else {
                nal_blob
            };
            let ts_bytes = muxer.mux_video_au(&payload, pts_90khz, keyframe);
            broadcaster.broadcast(Arc::new(ts_bytes));
        }

        encoded_frames += 1;
        if last_fps_log.elapsed() >= Duration::from_secs(5) {
            let fps = encoded_frames as f64 / last_fps_log.elapsed().as_secs_f64();
            log::info!(
                "H.264 pipeline {:.1} fps, {} subscribers",
                fps,
                broadcaster.subscriber_count()
            );
            encoded_frames = 0;
            last_fps_log = Instant::now();
        }
    }
    log::info!("H.264 pipeline exiting");
}

/// Scan an Annex-B H.264 byte stream for an IDR (NAL type 5).
fn annexb_has_idr(buf: &[u8]) -> bool {
    annexb_contains_nal_type(buf, 5)
}

fn annexb_contains_nal_type(buf: &[u8], target: u8) -> bool {
    for (_start, nal_type) in iter_nals(buf) {
        if nal_type == target {
            return true;
        }
    }
    false
}

/// Walk Annex-B start codes and yield (offset_of_nal_byte, nal_type).
pub fn iter_nals(buf: &[u8]) -> NalIter<'_> {
    NalIter { buf, pos: 0 }
}

pub struct NalIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for NalIter<'a> {
    type Item = (usize, u8);
    fn next(&mut self) -> Option<Self::Item> {
        let buf = self.buf;
        while self.pos + 3 < buf.len() {
            let i = self.pos;
            if buf[i] == 0 && buf[i + 1] == 0 {
                let nt_off = if buf[i + 2] == 0 && i + 3 < buf.len() && buf[i + 3] == 1 {
                    i + 4
                } else if buf[i + 2] == 1 {
                    i + 3
                } else {
                    self.pos += 1;
                    continue;
                };
                if nt_off >= buf.len() {
                    return None;
                }
                let nal_type = buf[nt_off] & 0x1F;
                self.pos = nt_off + 1;
                return Some((nt_off, nal_type));
            } else {
                self.pos += 1;
            }
        }
        None
    }
}

/// Extract the contiguous SPS+PPS region from an Annex-B blob, including
/// their start codes. Returns None if either is missing.
fn extract_sps_pps(buf: &[u8]) -> Option<Vec<u8>> {
    // We need positions of NAL bytes, then we walk back to the preceding
    // start code and slice through to the end of the unit.
    let mut sps_range: Option<(usize, usize)> = None;
    let mut pps_range: Option<(usize, usize)> = None;
    let nals: Vec<(usize, u8)> = iter_nals(buf).collect();
    for (idx, (nt_off, nal_type)) in nals.iter().enumerate() {
        let start = find_start_code_before(buf, *nt_off);
        let end = nals
            .get(idx + 1)
            .map(|(next_nt, _)| find_start_code_before(buf, *next_nt))
            .unwrap_or(buf.len());
        match nal_type {
            7 => sps_range = Some((start, end)),
            8 => pps_range = Some((start, end)),
            _ => {}
        }
    }
    match (sps_range, pps_range) {
        (Some((s1, e1)), Some((s2, e2))) => {
            let mut out = Vec::with_capacity((e1 - s1) + (e2 - s2));
            out.extend_from_slice(&buf[s1..e1]);
            out.extend_from_slice(&buf[s2..e2]);
            Some(out)
        }
        _ => None,
    }
}

/// Walk backwards from `nal_byte_offset` to find where the matching Annex-B
/// start code (00 00 01 or 00 00 00 01) begins.
fn find_start_code_before(buf: &[u8], nal_byte_offset: usize) -> usize {
    if nal_byte_offset >= 4
        && buf[nal_byte_offset - 4] == 0
        && buf[nal_byte_offset - 3] == 0
        && buf[nal_byte_offset - 2] == 0
        && buf[nal_byte_offset - 1] == 1
    {
        nal_byte_offset - 4
    } else if nal_byte_offset >= 3
        && buf[nal_byte_offset - 3] == 0
        && buf[nal_byte_offset - 2] == 0
        && buf[nal_byte_offset - 1] == 1
    {
        nal_byte_offset - 3
    } else {
        nal_byte_offset
    }
}
