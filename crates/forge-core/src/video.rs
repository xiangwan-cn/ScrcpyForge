use std::{sync::OnceLock, time::Instant};

use anyhow::{bail, Context, Result};
use ffmpeg::{
    codec,
    format::Pixel,
    frame,
    software::scaling::{context::Context as Scaler, flag::Flags},
};
use ffmpeg_next as ffmpeg;

use crate::protocol::stream::Codec;

extern "C" {
    fn forge_i420_to_rgb(yuv: *const u8, width: i32, height: i32, rgb: *mut u8) -> i32;
}

/// A decoded frame stored as compact I420. RGB and JPEG are materialized only
/// if a script or preview actually consumes this frame, then shared by all users.
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub presentation_time_us: i64,
    yuv: Vec<u8>,
    decoded_at: Instant,
    rgb: OnceLock<Vec<u8>>,
    jpeg: OnceLock<bytes::Bytes>,
}
impl VideoFrame {
    pub fn new_i420(width: u32, height: u32, yuv: Vec<u8>, presentation_time_us: i64) -> Self {
        Self {
            width,
            height,
            yuv,
            presentation_time_us,
            decoded_at: Instant::now(),
            rgb: OnceLock::new(),
            jpeg: OnceLock::new(),
        }
    }
    pub fn age_ms(&self) -> f64 {
        self.decoded_at.elapsed().as_secs_f64() * 1000.0
    }
    pub fn y_plane(&self) -> &[u8] {
        &self.yuv[..(self.width * self.height) as usize]
    }
    pub fn rgb(&self) -> Result<&[u8]> {
        if let Some(value) = self.rgb.get() {
            return Ok(value);
        }
        if self.width == 0 || self.height == 0 || self.width % 2 != 0 || self.height % 2 != 0 {
            bail!("I420 frame dimensions must be non-zero and even")
        }
        let expected = (self.width as usize * self.height as usize * 3) / 2;
        if self.yuv.len() != expected {
            bail!(
                "invalid I420 buffer: expected {expected} bytes, got {}",
                self.yuv.len()
            )
        }
        let mut value = vec![0u8; (self.width * self.height * 3) as usize];
        let code = unsafe {
            forge_i420_to_rgb(
                self.yuv.as_ptr(),
                self.width as i32,
                self.height as i32,
                value.as_mut_ptr(),
            )
        };
        if code != 0 {
            bail!("I420 to RGB conversion failed with code {code}")
        }
        let _ = self.rgb.set(value);
        Ok(self.rgb.get().expect("RGB cache initialized"))
    }
    pub fn jpeg(&self) -> Result<bytes::Bytes> {
        if let Some(value) = self.jpeg.get() {
            return Ok(value.clone());
        }
        let mut output = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 80).encode(
            self.rgb()?,
            self.width,
            self.height,
            image::ExtendedColorType::Rgb8,
        )?;
        let value = bytes::Bytes::from(output);
        let _ = self.jpeg.set(value.clone());
        Ok(self.jpeg.get().cloned().unwrap_or(value))
    }
}

pub struct FfmpegDecoder {
    decoder: ffmpeg::decoder::Video,
    scaler: Option<Scaler>,
    output_size: (u32, u32),
}
unsafe impl Send for FfmpegDecoder {}

impl FfmpegDecoder {
    pub fn new(codec_kind: Codec, width: u32, height: u32) -> Result<Self> {
        ffmpeg::init()?;
        let id = match codec_kind {
            Codec::H264 => codec::Id::H264,
            Codec::H265 => codec::Id::HEVC,
            Codec::Av1 => codec::Id::AV1,
        };
        let codec = codec::decoder::find(id).context("FFmpeg decoder unavailable")?;
        let decoder = codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()?;
        Ok(Self {
            decoder,
            scaler: None,
            output_size: (width, height),
        })
    }
    pub fn decode(&mut self, data: &[u8], pts_us: i64) -> Result<Vec<VideoFrame>> {
        let mut packet = ffmpeg::Packet::copy(data);
        packet.set_pts(Some(pts_us));
        self.decoder.send_packet(&packet)?;
        self.receive()
    }
    fn receive(&mut self) -> Result<Vec<VideoFrame>> {
        let mut frames = Vec::new();
        let mut decoded = frame::Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let size = (decoded.width(), decoded.height());
            let normalized = if decoded.format() != Pixel::YUV420P {
                if self.scaler.is_none() || self.output_size != size {
                    self.scaler = Some(Scaler::get(
                        decoded.format(),
                        size.0,
                        size.1,
                        Pixel::YUV420P,
                        size.0,
                        size.1,
                        Flags::FAST_BILINEAR,
                    )?);
                    self.output_size = size;
                }
                let mut yuv = frame::Video::new(Pixel::YUV420P, size.0, size.1);
                self.scaler.as_mut().unwrap().run(&decoded, &mut yuv)?;
                Some(yuv)
            } else {
                None
            };
            let source = normalized.as_ref().unwrap_or(&decoded);
            let cw = (size.0 + 1) / 2;
            let ch = (size.1 + 1) / 2;
            let mut packed = Vec::with_capacity((size.0 * size.1 + 2 * cw * ch) as usize);
            copy_plane(source, 0, size.0, size.1, &mut packed);
            copy_plane(source, 1, cw, ch, &mut packed);
            copy_plane(source, 2, cw, ch, &mut packed);
            frames.push(VideoFrame::new_i420(
                size.0,
                size.1,
                packed,
                decoded.pts().unwrap_or(0),
            ));
        }
        Ok(frames)
    }
}
fn copy_plane(frame: &frame::Video, index: usize, width: u32, height: u32, output: &mut Vec<u8>) {
    let stride = frame.stride(index);
    let data = frame.data(index);
    for row in 0..height as usize {
        let start = row * stride;
        output.extend_from_slice(&data[start..start + width as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn i420_rgb_is_lazy_cached_and_jpeg_is_valid() {
        let frame = VideoFrame::new_i420(2, 2, vec![128, 128, 128, 128, 128, 128], 0);
        let first = frame.rgb().unwrap().as_ptr();
        let rgb = frame.rgb().unwrap();
        assert_eq!(first, rgb.as_ptr());
        assert_eq!(rgb.len(), 12);
        assert!(rgb.iter().all(|v| v.abs_diff(130) <= 2));
        let jpeg = frame.jpeg().unwrap();
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        assert_eq!(jpeg.as_ptr(), frame.jpeg().unwrap().as_ptr());
    }
    #[test]
    fn malformed_i420_is_rejected() {
        assert!(VideoFrame::new_i420(2, 2, vec![0; 5], 0).rgb().is_err());
        assert!(VideoFrame::new_i420(3, 2, vec![0; 9], 0).rgb().is_err());
    }
}
