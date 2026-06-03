// Windows Media Foundation H.264 video encoder.
//
// Talks to the Microsoft H264 Encoder MFT directly via the IMFTransform
// interface. Picks the platform default encoder, which uses NVENC on
// NVIDIA GPUs, Quick Sync on Intel iGPUs and AMF on AMD - the same path
// any well-behaved capture/streaming app takes. Falls back to the software
// encoder if no hardware path is available.
//
// Input  : raw NV12 frames (Y plane followed by interleaved UV plane)
// Output : Annex-B H.264 byte stream (NAL units separated by 00 00 00 01)

#![cfg(windows)]

use anyhow::{Context, Result, anyhow, bail};
use std::mem::ManuallyDrop;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::*;
use windows::core::{GUID, Interface};

const MF_VERSION_LOCAL: u32 = 0x0002_0070;

/// Per-process Media Foundation init refcount. MFStartup must be balanced
/// by MFShutdown; we wrap with a static guard so multiple encoder instances
/// only call it once.
struct MfStartup;

impl MfStartup {
    fn ensure() -> Result<()> {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        static mut OK: bool = false;
        ONCE.call_once(|| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            match MFStartup(MF_VERSION_LOCAL, MFSTARTUP_NOSOCKET) {
                Ok(()) => OK = true,
                Err(e) => log::error!("MFStartup failed: {e}"),
            }
        });
        if unsafe { OK } {
            Ok(())
        } else {
            bail!("Media Foundation could not be initialised")
        }
    }
}

/// One configured H.264 encoder transform. Not Send because the underlying
/// IMFTransform is COM-thread-bound; create and use on the same thread.
pub struct H264Encoder {
    transform: IMFTransform,
    /// Same encoder, queried as ICodecAPI for codec-specific runtime knobs
    /// like force-keyframe and GOP size. None if the MFT does not expose it.
    codec_api: Option<ICodecAPI>,
    pub width: u32,
    pub height: u32,
    fps_num: u32,
    fps_den: u32,
    out_stream_id: u32,
    in_stream_id: u32,
    /// Annex-B SPS/PPS prefix to prepend to the first IDR. Some MFTs put
    /// them inline already, in which case this is empty.
    pub extra_data: Vec<u8>,
    /// How many frames between forced keyframes. Re-emitted via
    /// MFSampleExtension_CleanPoint on the input sample because the MSMF
    /// software encoder ignores CODECAPI_AVEncMPVGOPSize set on its
    /// attribute store.
    keyframe_interval: u32,
    frames_since_idr: u32,
}

impl H264Encoder {
    /// Build an encoder for `width x height` frames at the given frame rate
    /// and average bitrate (bits/second). Picks the platform default H264
    /// MFT and prefers hardware over software.
    pub fn new(width: u32, height: u32, fps: u32, bitrate_bps: u32) -> Result<Self> {
        MfStartup::ensure()?;

        let transform = pick_h264_encoder()?;

        let codec_api: Option<ICodecAPI> = transform.cast().ok();
        log::info!("ICodecAPI available: {}", codec_api.is_some());

        // Output type first: H264, target resolution + fps + bitrate.
        // For most MFTs the output type must be set BEFORE the input type,
        // because the input type validation depends on what we promised the
        // encoder would produce.
        let out_type: IMFMediaType = unsafe { MFCreateMediaType()? };
        unsafe {
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps)?;
            set_size(&out_type, MF_MT_FRAME_SIZE, width, height)?;
            set_ratio(&out_type, MF_MT_FRAME_RATE, fps, 1)?;
            set_ratio(&out_type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            out_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            // Baseline / Main / High. Most second-PC OBS consumers handle
            // High. If we hit compatibility issues we can drop to Main.
            out_type.SetUINT32(
                &MF_MT_MPEG2_PROFILE,
                eAVEncH264VProfile_Main.0 as u32,
            )?;
            transform
                .SetOutputType(0, &out_type, 0)
                .context("SetOutputType H264")?;
        }

        // Input type: NV12 with same resolution and fps.
        let in_type: IMFMediaType = unsafe { MFCreateMediaType()? };
        unsafe {
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            set_size(&in_type, MF_MT_FRAME_SIZE, width, height)?;
            set_ratio(&in_type, MF_MT_FRAME_RATE, fps, 1)?;
            set_ratio(&in_type, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
            in_type.SetUINT32(
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u32,
            )?;
            transform
                .SetInputType(0, &in_type, 0)
                .context("SetInputType NV12")?;
        }

        // Some MFTs report multiple input / output streams; the H264 encoder
        // only ever has one of each so the IDs are always 0, but we go
        // through the proper enum for cleanliness.
        let mut in_ids = [0u32; 1];
        let mut out_ids = [0u32; 1];
        let _ = unsafe { transform.GetStreamIDs(&mut in_ids, &mut out_ids) };
        let in_stream_id = in_ids[0];
        let out_stream_id = out_ids[0];

        // Codec parameters work AFTER both input and output types are set.
        if let Some(api) = codec_api.as_ref() {
            let try_set = |name: &str, key: &GUID, var: &windows::Win32::System::Variant::VARIANT| {
                let r = unsafe { api.SetValue(key, var) };
                match r {
                    Ok(()) => log::info!("ICodecAPI {name}: ok"),
                    Err(e) => log::warn!("ICodecAPI {name}: {e}"),
                }
            };
            try_set(
                "AVEncCommonRateControlMode",
                &CODECAPI_AVEncCommonRateControlMode,
                &propvar_u32(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            try_set("AVEncCommonMeanBitRate", &CODECAPI_AVEncCommonMeanBitRate, &propvar_u32(bitrate_bps));
            try_set("AVEncMPVGOPSize", &CODECAPI_AVEncMPVGOPSize, &propvar_u32(fps.max(1)));
            try_set("AVEncMPVDefaultBPictureCount", &CODECAPI_AVEncMPVDefaultBPictureCount, &propvar_u32(0));
        }

        unsafe {
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }

        Ok(Self {
            transform,
            codec_api,
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            in_stream_id,
            out_stream_id,
            extra_data: Vec::new(),
            keyframe_interval: fps.max(1),
            frames_since_idr: u32::MAX,
        })
    }

    /// Push one NV12 frame with the given monotonic timestamp (100ns units)
    /// and drain whatever the encoder is ready to spit out. Returns 0 or
    /// more Annex-B byte blobs.
    pub fn encode(&mut self, nv12: &[u8], timestamp_100ns: i64) -> Result<Vec<Vec<u8>>> {
        if nv12.len() < (self.width as usize) * (self.height as usize) * 3 / 2 {
            bail!("NV12 frame shorter than expected: {} bytes", nv12.len());
        }
        // Wrap nv12 in an IMFMediaBuffer and IMFSample.
        let buf_size = nv12.len() as u32;
        let buf: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(buf_size)? };
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len: u32 = 0;
            let mut cur_len: u32 = 0;
            buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buf.Unlock()?;
            buf.SetCurrentLength(buf_size)?;
        }
        let sample: IMFSample = unsafe { MFCreateSample()? };
        unsafe {
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(timestamp_100ns)?;
            let dur = (10_000_000i64 * self.fps_den as i64) / self.fps_num.max(1) as i64;
            sample.SetSampleDuration(dur)?;
            // Force a fresh IDR every `keyframe_interval` frames so clients
            // joining mid-stream can sync quickly. ICodecAPI is the only path
            // that the Microsoft H.264 software encoder actually honours.
            self.frames_since_idr = self.frames_since_idr.saturating_add(1);
            if self.frames_since_idr >= self.keyframe_interval {
                if let Some(api) = self.codec_api.as_ref() {
                    let _ = api.SetValue(
                        &CODECAPI_AVEncVideoForceKeyFrame,
                        &propvar_u32(1),
                    );
                }
                self.frames_since_idr = 0;
            }
            self.transform
                .ProcessInput(self.in_stream_id, &sample, 0)
                .context("ProcessInput")?;
        }

        // Drain.
        let mut out: Vec<Vec<u8>> = Vec::new();
        loop {
            let info = unsafe {
                self.transform.GetOutputStreamInfo(self.out_stream_id)?
            };
            // Allocate a sample if the encoder does not provide one itself.
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let mut out_data = MFT_OUTPUT_DATA_BUFFER::default();
            out_data.dwStreamID = self.out_stream_id;
            let mut owned_sample: Option<IMFSample> = None;
            if !provides {
                let sz = info.cbSize.max(1024 * 1024);
                let out_buf: IMFMediaBuffer = unsafe { MFCreateMemoryBuffer(sz)? };
                let sample: IMFSample = unsafe { MFCreateSample()? };
                unsafe { sample.AddBuffer(&out_buf)? };
                out_data.pSample = ManuallyDrop::new(Some(sample.clone()));
                owned_sample = Some(sample);
            }
            let mut status: u32 = 0;
            let hr = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out_data), &mut status)
            };
            match hr {
                Ok(()) => {
                    let sample = unsafe { out_data.pSample.take() }
                        .or(owned_sample)
                        .ok_or_else(|| anyhow!("MFT ProcessOutput returned no sample"))?;
                    let bytes = read_sample_bytes(&sample)?;
                    if !bytes.is_empty() {
                        out.push(bytes);
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Encoder is renegotiating output format; we ignore and
                    // retry next call.
                    break;
                }
                Err(e) => return Err(anyhow!("ProcessOutput failed: {e}")),
            }
        }
        Ok(out)
    }
}

fn pick_h264_encoder() -> Result<IMFTransform> {
    // MFT_CATEGORY_VIDEO_ENCODER, hardware preferred, async OK, no transcoder.
    let category = MFT_CATEGORY_VIDEO_ENCODER;
    let in_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    // Only sync MFTs. The NVIDIA / Intel hardware encoders register as
    // async-only and need the IMFMediaEventGenerator pattern, which this
    // encoder does not implement. Falling back to the Microsoft software
    // H.264 encoder is slower but correct; an async path can land later.
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(&in_type),
            Some(&out_type),
            &mut activates,
            &mut count,
        )
        .context("MFTEnumEx for H264 encoder")?;
    }
    if count == 0 || activates.is_null() {
        bail!("no H264 encoder MFT available");
    }
    let result = unsafe {
        let slice = std::slice::from_raw_parts(activates, count as usize);
        let activate = slice
            .iter()
            .find_map(|a| a.clone())
            .ok_or_else(|| anyhow!("MFTEnumEx returned null entries"))?;
        let transform: IMFTransform = activate
            .ActivateObject::<IMFTransform>()
            .context("ActivateObject IMFTransform")?;
        windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
        transform
    };
    log_encoder_name(&result);
    Ok(result)
}

fn log_encoder_name(transform: &IMFTransform) {
    let attrs = unsafe { transform.GetAttributes() };
    if let Ok(attrs) = attrs {
        // MFT_FRIENDLY_NAME_Attribute is the human-readable display name.
        let len = unsafe {
            attrs.GetStringLength(&MFT_FRIENDLY_NAME_Attribute).unwrap_or(0)
        };
        if len > 0 {
            let mut buf = vec![0u16; (len + 1) as usize];
            let mut written: u32 = 0;
            unsafe {
                let _ = attrs.GetString(
                    &MFT_FRIENDLY_NAME_Attribute,
                    &mut buf,
                    Some(&mut written),
                );
            }
            let name = String::from_utf16_lossy(&buf[..written as usize]);
            log::info!("H264 encoder: {name}");
        }
    }
}

fn set_size(t: &IMFMediaType, key: GUID, w: u32, h: u32) -> Result<()> {
    let packed = ((w as u64) << 32) | (h as u64);
    unsafe { t.SetUINT64(&key, packed)? };
    Ok(())
}

fn set_ratio(t: &IMFMediaType, key: GUID, num: u32, den: u32) -> Result<()> {
    let packed = ((num as u64) << 32) | (den as u64);
    unsafe { t.SetUINT64(&key, packed)? };
    Ok(())
}

/// Build a VARIANT carrying a u32 (VT_UI4). ICodecAPI takes its parameter
/// values through this Win32 tagged-union type. Note that SetValue uses
/// VARIANT not PROPVARIANT; the field layouts differ slightly.
fn propvar_u32(v: u32) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARENUM, VARIANT};
    unsafe {
        let mut p: VARIANT = std::mem::zeroed();
        let inner = &mut p.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_UI4.0);
        inner.Anonymous.ulVal = v;
        p
    }
}

fn propvar_bool(b: bool) -> windows::Win32::System::Variant::VARIANT {
    use windows::Win32::System::Variant::{VARENUM, VARIANT};
    unsafe {
        let mut p: VARIANT = std::mem::zeroed();
        let inner = &mut p.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_BOOL.0);
        // VARIANT_BOOL is a newtype around i16. -1 = true, 0 = false.
        // Bit-equivalent to writing the i16 directly; transmute keeps the
        // dependency on the exact wrapper out of the call sites.
        let v: i16 = if b { -1 } else { 0 };
        std::ptr::write(&mut inner.Anonymous.boolVal as *mut _ as *mut i16, v);
        p
    }
}

fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
    unsafe {
        let total_len = sample.GetTotalLength()?;
        let buf: IMFMediaBuffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max_len: u32 = 0;
        let mut cur_len: u32 = 0;
        buf.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))?;
        let len = cur_len.min(total_len) as usize;
        let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
        buf.Unlock()?;
        Ok(bytes)
    }
}
