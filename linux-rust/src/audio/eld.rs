//! AAC-ELD decoder using libavcodec (LGPL) for the AirPods proprietary hi-res uplink.
//!   AOT 39, mono, 64000 Hz, 480-sample frame (7.5 ms), ~80 kbps VBR.
//! See https://ffmpeg-d.dpldocs.info/v3.1.1/ffmpeg.libavcodec.avcodec.AVCodecContext.html

use ffmpeg_sys_next as ff;
use std::os::raw::c_int;
use std::ptr;

pub const ELD_SAMPLE_RATE: u32 = 64000;
pub const ELD_FRAME_SAMPLES: usize = 480;
pub const ELD_CHANNELS: i32 = 1;

const ELD_ASC: [u8; 4] = [0xF8, 0xE6, 0x30, 0x00];
const ELD_CODING_RATE: c_int = 48000;
const ELD_INBUF_MAX: usize = 512;

const PAD: usize = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;

pub struct EldDecoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    inbuf: Vec<u8>,
}

// SAFETY: the FFmpeg objects are owned exclusively by this struct and have no
// thread affinity; `&mut self` on every method means one thread uses them at a time.
unsafe impl Send for EldDecoder {}

#[inline]
fn f_to_s16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

impl EldDecoder {
    // Open the AAC-ELD decoder. Returns None on failure.
    pub fn new() -> Option<Self> {
        // SAFETY: every pointer is null-checked right after allocation before it is
        // dereferenced; extradata is allocated with the padding FFmpeg requires and
        // ownership passes to ctx, which frees it in avcodec_free_context.
        unsafe {
            // stop libavcodec from spamming stderr
            ff::av_log_set_level(ff::AV_LOG_FATAL);

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
            };

            // extradata = ASC
            let extradata = ff::av_mallocz(ELD_ASC.len() + PAD) as *mut u8;
            if extradata.is_null() {
                d.free();
                return None;
            }
            ptr::copy_nonoverlapping(ELD_ASC.as_ptr(), extradata, ELD_ASC.len());
            (*ctx).extradata = extradata;
            (*ctx).extradata_size = ELD_ASC.len() as c_int;
            (*ctx).sample_rate = ELD_CODING_RATE;
            // FFmpeg < 5.1 (Ubuntu 22.04 ships 4.4) has no AVChannelLayout, only the
            // legacy count and mask. A negative mask cannot happen; 0 means "unspecified".
            (*ctx).channels = ELD_CHANNELS;
            (*ctx).channel_layout =
                u64::try_from(ff::av_get_default_channel_layout(ELD_CHANNELS)).unwrap_or(0);

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
                ff::av_frame_free(&mut self.frame);
            }
            if !self.pkt.is_null() {
                ff::av_packet_free(&mut self.pkt);
            }
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
        }
    }

    // Decode one access unit, appending interleaved i16 PCM to `out`.
    // Returns the number of samples appended, or None on a decode error.
    pub fn decode(&mut self, au: &[u8], out: &mut Vec<i16>) -> Option<usize> {
        if au.is_empty() || au.len() > ELD_INBUF_MAX {
            return None;
        }
        // SAFETY: ctx/pkt/frame are live (new() only returns a fully built decoder).
        // au.len() <= ELD_INBUF_MAX, so the copy plus PAD zero bytes fits inbuf. The
        // packet is not refcounted, so avcodec_send_packet copies the data. Plane
        // pointers are read only for the reported format, channel count and
        // nb_samples, which FFmpeg guarantees are backed by the frame buffers.
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

            let nch = (*self.frame).channels;
            let ns = (*self.frame).nb_samples;
            if nch <= 0 || ns <= 0 {
                return None;
            }
            let (nch, ns) = (nch as usize, ns as usize);
            let total = ns * nch;
            out.reserve(total);

            let data = &(*self.frame).data;
            match (*self.frame).format {
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as c_int => {
                    // native aac: planar float
                    for i in 0..ns {
                        for c in 0..nch {
                            let plane = data[c] as *const f32;
                            out.push(f_to_s16(*plane.add(i)));
                        }
                    }
                }
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_FLT as c_int => {
                    let p = data[0] as *const f32;
                    for i in 0..total {
                        out.push(f_to_s16(*p.add(i)));
                    }
                }
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_S16P as c_int => {
                    for i in 0..ns {
                        for c in 0..nch {
                            let plane = data[c] as *const i16;
                            out.push(*plane.add(i));
                        }
                    }
                }
                f if f == ff::AVSampleFormat::AV_SAMPLE_FMT_S16 as c_int => {
                    let p = data[0] as *const i16;
                    for i in 0..total {
                        out.push(*p.add(i));
                    }
                }
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
