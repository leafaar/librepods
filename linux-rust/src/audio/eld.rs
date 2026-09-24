//! AAC-ELD decoder using libavcodec (LGPL) for the AirPods proprietary hi-res uplink.
//!   AOT 39, mono, 64000 Hz, 480-sample frame (7.5 ms), ~80 kbps VBR.
//! See https://ffmpeg-d.dpldocs.info/v3.1.1/ffmpeg.libavcodec.avcodec.AVCodecContext.html

use {
    ffmpeg_sys_next as ff,
    std::{os::raw::c_int, ptr, sync::Once},
    tracing::{debug, warn},
};

pub const ELD_SAMPLE_RATE: u32 = 64000;
pub const ELD_FRAME_SAMPLES: usize = 480;
pub const ELD_CHANNELS: i32 = 1;

const ELD_ASC: [u8; 4] = [0xF8, 0xE6, 0x30, 0x00];
const ELD_CODING_RATE: c_int = 48000;
const ELD_INBUF_MAX: usize = 512;

const PAD: usize = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;

// av_log_set_level sets a process-wide global, so it only needs to run once.
static QUIET_AV_LOG: Once = Once::new();

pub struct EldDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    inbuf: Vec<u8>,
    // Set once the first frame's output sample rate has been logged.
    rate_logged: bool,
}

// SAFETY: the FFmpeg objects are owned exclusively by this struct and have no
// thread affinity; `&mut self` on every method means one thread uses them at a time.
unsafe impl Send for EldDecoder {}

/// A frame's byte plane pointer as a pointer to its samples.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "FFmpeg allocates frame planes aligned for SIMD (at least 16 bytes), more than any sample type needs"
)]
fn sample_ptr<T>(plane: *mut u8) -> *const T {
    plane.cast::<T>().cast_const()
}

#[inline]
fn f_to_s16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl EldDecoder {
    // Open the AAC-ELD decoder. Returns None on failure.
    pub fn new() -> Option<Self> {
        // Stop libavcodec from spamming stderr.
        QUIET_AV_LOG.call_once(|| {
            // SAFETY: av_log_set_level only stores an int in libavutil's global log
            // level; it takes no pointers and has no other precondition.
            unsafe { ff::av_log_set_level(ff::AV_LOG_FATAL) };
        });

        // SAFETY: every pointer is null-checked right after allocation before it is
        // dereferenced; extradata is allocated with the padding FFmpeg requires and
        // ownership passes to ctx, which frees it in avcodec_free_context.
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_AAC);
            if codec.is_null() {
                return None;
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return None;
            }

            let mut d = EldDecoder {
                ctx,
                pkt: ptr::null_mut(),
                frame: ptr::null_mut(),
                inbuf: vec![0u8; ELD_INBUF_MAX + PAD],
                rate_logged: false,
            };

            // extradata = ASC
            let extradata = ff::av_mallocz(ELD_ASC.len() + PAD).cast::<u8>();
            if extradata.is_null() {
                d.free();
                return None;
            }
            ptr::copy_nonoverlapping(ELD_ASC.as_ptr(), extradata, ELD_ASC.len());
            (*ctx).extradata = extradata;
            (*ctx).extradata_size = ELD_ASC.len() as c_int;
            (*ctx).sample_rate = ELD_CODING_RATE;
            // build.rs selects the channel API: AVChannelLayout from FFmpeg 5.1,
            // the legacy count and mask before it (they were removed in 7.0).
            #[cfg(ffmpeg_ch_layout)]
            ff::av_channel_layout_default(&raw mut (*ctx).ch_layout, ELD_CHANNELS);
            #[cfg(not(ffmpeg_ch_layout))]
            {
                (*ctx).channels = ELD_CHANNELS;
                // A negative mask cannot happen; 0 means "unspecified".
                (*ctx).channel_layout =
                    u64::try_from(ff::av_get_default_channel_layout(ELD_CHANNELS)).unwrap_or(0);
            }

            if ff::avcodec_open2(ctx, codec, ptr::null_mut()) < 0 {
                d.free();
                return None;
            }

            d.pkt = ff::av_packet_alloc();
            d.frame = ff::av_frame_alloc();
            if d.pkt.is_null() || d.frame.is_null() {
                d.free();
                return None;
            }
            Some(d)
        }
    }

    // Free all FFmpeg resources. The av_*_free calls null the pointers they take,
    // so a second call (Drop after a failed new) is a no-op.
    fn free(&mut self) {
        // SAFETY: each pointer is either null or owned by self and still live.
        unsafe {
            if !self.frame.is_null() {
                ff::av_frame_free(&raw mut self.frame);
            }
            if !self.pkt.is_null() {
                ff::av_packet_free(&raw mut self.pkt);
            }
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&raw mut self.ctx);
            }
        }
    }

    // Decode one access unit, appending interleaved i16 PCM to `out`.
    // Returns the number of samples appended, or None on a decode error or a frame
    // whose channel count is not ELD_CHANNELS.
    pub fn decode(&mut self, au: &[u8], out: &mut Vec<i16>) -> Option<usize> {
        if au.is_empty() || au.len() > ELD_INBUF_MAX {
            return None;
        }
        // SAFETY: ctx/pkt/frame are live (new() only returns a fully built decoder).
        // au.len() <= ELD_INBUF_MAX, so the copy plus PAD zero bytes fits inbuf. The
        // packet is not refcounted, so avcodec_send_packet copies the data. Plane
        // pointers are read only for the reported format and nb_samples, and only
        // when the channel count equals ELD_CHANNELS, so every plane index stays
        // inside the 8-entry data array and is backed by the frame buffers.
        unsafe {
            self.inbuf[..au.len()].copy_from_slice(au);
            self.inbuf[au.len()..au.len() + PAD].fill(0);
            (*self.pkt).data = self.inbuf.as_mut_ptr();
            (*self.pkt).size = au.len() as c_int;

            if ff::avcodec_send_packet(self.ctx, self.pkt) < 0 {
                return None;
            }
            if ff::avcodec_receive_frame(self.ctx, self.frame) < 0 {
                return None;
            }

            #[cfg(ffmpeg_ch_layout)]
            let nch = (*self.frame).ch_layout.nb_channels;
            #[cfg(not(ffmpeg_ch_layout))]
            let nch = (*self.frame).channels;
            let ns = (*self.frame).nb_samples;
            if nch != ELD_CHANNELS || ns <= 0 {
                return None;
            }
            if !self.rate_logged {
                self.rate_logged = true;
                // The rate in the AudioSpecificConfig header reads 48 kHz, but the
                // AirPods send 133 frames of 480 samples a second, so the stream
                // runs at 64 kHz. The frame size is what has to match.
                let rate = (*self.frame).sample_rate;
                if usize::try_from(ns) == Ok(ELD_FRAME_SAMPLES) {
                    debug!(
                        "[audio] AAC-ELD frames of {} samples (header rate {} Hz, played at {} Hz)",
                        ns, rate, ELD_SAMPLE_RATE
                    );
                } else {
                    warn!(
                        "[audio] AAC-ELD frames of {} samples, expected {}; the microphone will play at the wrong speed",
                        ns, ELD_FRAME_SAMPLES
                    );
                }
            }
            let (nch, ns) = (nch as usize, ns as usize);
            let total = ns * nch;
            out.reserve(total);

            let data = &(*self.frame).data;
            let planes = &data[..nch];
            match (*self.frame).format {
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
                    // native aac: planar float
                    for i in 0..ns {
                        for &plane in planes {
                            out.push(f_to_s16(*sample_ptr::<f32>(plane).add(i)));
                        }
                    }
                },
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
                    let p = sample_ptr::<f32>(data[0]);
                    for i in 0..total {
                        out.push(f_to_s16(*p.add(i)));
                    }
                },
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
                    for i in 0..ns {
                        for &plane in planes {
                            out.push(*sample_ptr::<i16>(plane).add(i));
                        }
                    }
                },
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
                    let p = sample_ptr::<i16>(data[0]);
                    for i in 0..total {
                        out.push(*p.add(i));
                    }
                },
                _ => return None,
            }
            Some(total)
        }
    }
}

impl Drop for EldDecoder {
    fn drop(&mut self) {
        self.free();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_samples_are_clamped_and_rounded_to_s16() {
        assert_eq!(f_to_s16(0.0), 0);
        assert_eq!(f_to_s16(1.0), 32767);
        assert_eq!(f_to_s16(-1.0), -32767);
        assert_eq!(f_to_s16(2.5), 32767);
        assert_eq!(f_to_s16(-7.0), -32767);
        assert_eq!(f_to_s16(0.5), 16384);
    }

    #[test]
    fn decoder_rejects_empty_and_oversized_access_units() {
        // Needs libavcodec's AAC decoder, which the binary links anyway.
        let Some(mut decoder) = EldDecoder::new() else {
            return;
        };
        let mut out = Vec::new();

        assert_eq!(decoder.decode(&[], &mut out), None);
        assert_eq!(decoder.decode(&[0; ELD_INBUF_MAX + 1], &mut out), None);
        assert!(out.is_empty());
    }
}
