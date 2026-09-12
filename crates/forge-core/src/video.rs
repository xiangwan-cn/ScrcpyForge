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

const MAX_VIDEO_DIMENSION: u32 = 8192;

extern "C" {
    fn forge_i420_to_rgb(yuv: *const u8, width: i32, height: i32, rgb: *mut u8) -> i32;
    fn forge_i420_roi_to_rgb(
        yuv: *const u8,
        width: i32,
        height: i32,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        rgb: *mut u8,
    ) -> i32;
}

/// A decoded frame stored as compact I420. RGB and JPEG are materialized only
/// if a script or preview actually consumes this frame, then shared by all users.
#[derive(Debug)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub presentation_time_us: i64,
    /// Monotonically increasing sequence assigned by the decoder. A strategy
    /// rescan can reuse the same frame without pretending a new video frame
    /// arrived.
    pub frame_seq: u64,
    yuv: Vec<u8>,
    decoded_at: Instant,
    rgb: OnceLock<std::result::Result<Vec<u8>, String>>,
    jpeg: OnceLock<std::result::Result<bytes::Bytes, String>>,
}

/// RGB pixels for an even-aligned ROI and the expanded `(x1, y1, x2, y2)`
/// coordinates used to produce them.
pub type RgbRoi = (Vec<u8>, (u32, u32, u32, u32));

impl VideoFrame {
    pub fn new_i420(width: u32, height: u32, yuv: Vec<u8>, presentation_time_us: i64) -> Self {
        Self {
            width,
            height,
            yuv,
            presentation_time_us,
            frame_seq: 0,
            decoded_at: Instant::now(),
            rgb: OnceLock::new(),
            jpeg: OnceLock::new(),
        }
    }
    pub fn age_ms(&self) -> f64 {
        self.decoded_at.elapsed().as_secs_f64() * 1000.0
    }
    pub fn with_sequence(mut self, frame_seq: u64) -> Self {
        self.frame_seq = frame_seq;
        self
    }
    fn validate_i420(&self) -> Result<usize> {
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || self.width > i32::MAX as u32
            || self.height > i32::MAX as u32
            || self.width > MAX_VIDEO_DIMENSION
            || self.height > MAX_VIDEO_DIMENSION
        {
            bail!("I420 frame dimensions must be non-zero and even")
        }
        let y_size = (self.width as usize)
            .checked_mul(self.height as usize)
            .context("I420 frame dimensions overflow")?;
        let chroma_size = (self.width as usize / 2)
            .checked_mul(self.height as usize / 2)
            .and_then(|size| size.checked_mul(2))
            .context("I420 frame dimensions overflow")?;
        let expected = y_size
            .checked_add(chroma_size)
            .context("I420 frame dimensions overflow")?;
        if self.yuv.len() != expected {
            bail!(
                "invalid I420 buffer: expected {expected} bytes, got {}",
                self.yuv.len()
            )
        }
        Ok(y_size)
    }
    /// Borrow the luma plane using the original zero-copy API. Callers that
    /// need validation should use the checked helper paths (`rgb`, `pixel_rgb`
    /// or `luma_signature`) before trusting malformed input.
    pub fn y_plane(&self) -> &[u8] {
        let size = (self.width as usize).saturating_mul(self.height as usize);
        &self.yuv[..size.min(self.yuv.len())]
    }
    pub(crate) fn y_plane_checked(&self) -> Result<&[u8]> {
        let size = self.validate_i420()?;
        Ok(&self.yuv[..size])
    }
    /// Return a cheap, deterministic fingerprint of the luma plane. It is used
    /// as an optional scene-change gate; a small mean/variance sample in each
    /// tile catches local UI changes while remaining far cheaper than
    /// converting the frame to RGB. Quantization absorbs small codec
    /// fluctuations on static screens.
    pub fn luma_signature(&self) -> Result<u64> {
        let y = self.y_plane_checked()?;
        let mut hash = 0xcbf29ce484222325u64;
        const TILES_X: u32 = 16;
        const TILES_Y: u32 = 9;
        for tile_y in 0..TILES_Y {
            for tile_x in 0..TILES_X {
                let x0 = tile_x * self.width / TILES_X;
                let x1 = ((tile_x + 1) * self.width / TILES_X)
                    .max(x0 + 1)
                    .min(self.width);
                let y0 = tile_y * self.height / TILES_Y;
                let y1 = ((tile_y + 1) * self.height / TILES_Y)
                    .max(y0 + 1)
                    .min(self.height);
                let mut sum = 0u64;
                let mut square_sum = 0u64;
                for sample_y in 0..2 {
                    let y_pos = (y0 + ((y1 - y0) * (sample_y + 1) / 3)).min(y1 - 1);
                    for sample_x in 0..2 {
                        let x_pos = (x0 + ((x1 - x0) * (sample_x + 1) / 3)).min(x1 - 1);
                        let value = y[y_pos as usize * self.width as usize + x_pos as usize] as u64;
                        sum += value;
                        square_sum += value * value;
                    }
                }
                // Four samples per tile. The numerator avoids floating point
                // drift and is small enough to stay within u64 at any
                // supported frame size.
                let variance_numerator = square_sum * 4 - sum * sum;
                hash ^= (sum / 4) >> 3;
                hash = hash.wrapping_mul(0x100000001b3);
                hash ^= (variance_numerator / 16) >> 3;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        hash ^= self.width as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        Ok(hash ^ self.height as u64)
    }
    pub fn pixel_rgb(&self, x: u32, y: u32) -> Result<(u8, u8, u8)> {
        if x >= self.width || y >= self.height {
            bail!("pixel out of bounds")
        }
        let y_size = self.validate_i420()?;
        let chroma_width = (self.width / 2) as usize;
        let chroma_height = (self.height / 2) as usize;
        // Match OpenCV's COLOR_YUV2RGB_I420 conversion used by rgb(): I420
        // carries studio-range luma (16..235) and centered chroma.
        let luma_index = (y as usize)
            .checked_mul(self.width as usize)
            .and_then(|value| value.checked_add(x as usize))
            .context("pixel index overflow")?;
        let luma = (self.yuv[luma_index] as f32 - 16.0).max(0.0);
        let chroma_index = (y as usize / 2) * chroma_width + x as usize / 2;
        let u = self.yuv[y_size + chroma_index] as f32 - 128.0;
        let v = self.yuv[y_size + chroma_width * chroma_height + chroma_index] as f32 - 128.0;
        let clamp = |value: f32| value.round().clamp(0.0, 255.0) as u8;
        Ok((
            clamp(1.164_383 * luma + 1.596_027 * v),
            clamp(1.164_383 * luma - 0.391_762 * u - 0.812_968 * v),
            clamp(1.164_383 * luma + 2.017_232 * u),
        ))
    }
    pub fn rgb(&self) -> Result<&[u8]> {
        let value = self.rgb.get_or_init(|| {
            let result = (|| -> Result<Vec<u8>> {
                self.validate_i420()?;
                let rgb_len = (self.width as usize)
                    .checked_mul(self.height as usize)
                    .and_then(|size| size.checked_mul(3))
                    .context("RGB frame dimensions overflow")?;
                let mut value = vec![0u8; rgb_len];
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
                Ok(value)
            })();
            result.map_err(|error| error.to_string())
        });
        match value {
            Ok(rgb) => Ok(rgb),
            Err(error) => bail!("I420 to RGB conversion failed: {error}"),
        }
    }
    /// Convert only an even-aligned ROI. I420 chroma is sampled for each 2x2
    /// luma block, so the requested rectangle is expanded by at most one pixel
    /// on each edge and the returned offset describes that expanded image.
    pub fn rgb_roi(&self, roi: (u32, u32, u32, u32)) -> Result<RgbRoi> {
        self.validate_i420()?;
        let (x1, y1, x2, y2) = roi;
        if x1 >= x2 || y1 >= y2 || x2 > self.width || y2 > self.height {
            bail!("invalid RGB ROI")
        }
        let ax1 = x1 & !1;
        let ay1 = y1 & !1;
        let ax2 = (x2.saturating_add(1) & !1).min(self.width);
        let ay2 = (y2.saturating_add(1) & !1).min(self.height);
        if ax1 >= ax2 || ay1 >= ay2 {
            bail!("invalid RGB ROI")
        }
        let roi_width = ax2 - ax1;
        let roi_height = ay2 - ay1;
        let output_len = (roi_width as usize)
            .checked_mul(roi_height as usize)
            .and_then(|size| size.checked_mul(3))
            .context("RGB ROI dimensions overflow")?;
        let mut output = vec![0u8; output_len];
        let code = unsafe {
            forge_i420_roi_to_rgb(
                self.yuv.as_ptr(),
                self.width as i32,
                self.height as i32,
                ax1 as i32,
                ay1 as i32,
                ax2 as i32,
                ay2 as i32,
                output.as_mut_ptr(),
            )
        };
        if code != 0 {
            bail!("I420 ROI to RGB conversion failed with code {code}")
        }
        Ok((output, (ax1, ay1, ax2, ay2)))
    }
    pub fn jpeg(&self) -> Result<bytes::Bytes> {
        let value = self.jpeg.get_or_init(|| {
            let result = (|| -> Result<bytes::Bytes> {
                let mut output = Vec::new();
                image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, 80).encode(
                    self.rgb()?,
                    self.width,
                    self.height,
                    image::ExtendedColorType::Rgb8,
                )?;
                Ok(bytes::Bytes::from(output))
            })();
            result.map_err(|error| error.to_string())
        });
        match value {
            Ok(bytes) => Ok(bytes.clone()),
            Err(error) => bail!("JPEG encoding failed: {error}"),
        }
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
        if width == 0
            || height == 0
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || width > MAX_VIDEO_DIMENSION
            || height > MAX_VIDEO_DIMENSION
        {
            bail!("decoder dimensions are outside the supported range")
        }
        ffmpeg::init()?;
        let id = match codec_kind {
            Codec::H264 => codec::Id::H264,
            Codec::H265 => codec::Id::HEVC,
            Codec::Av1 => codec::Id::AV1,
        };
        let codec = codec::decoder::find(id).context("FFmpeg decoder unavailable")?;
        let mut context = codec::context::Context::new_with_codec(codec);
        let threads = std::env::var("SCRCPYFORGE_DECODE_THREADS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|value| value.get().min(2))
                    .unwrap_or(2)
            })
            .clamp(1, 32);
        let kind = match std::env::var("SCRCPYFORGE_DECODE_THREAD_TYPE")
            .ok()
            .as_deref()
        {
            Some("frame") => ffmpeg::codec::threading::Type::Frame,
            Some("none") => ffmpeg::codec::threading::Type::None,
            _ => ffmpeg::codec::threading::Type::Slice,
        };
        let mut threading = ffmpeg::codec::threading::Config::count(threads);
        threading.kind = kind;
        context.set_threading(threading);
        let decoder = context.decoder().video()?;
        Ok(Self {
            decoder,
            scaler: None,
            output_size: (width, height),
        })
    }
    pub fn decode(&mut self, data: &[u8], pts_us: i64) -> Result<Vec<VideoFrame>> {
        let mut packet = ffmpeg::Packet::copy(data);
        packet.set_pts(Some(pts_us));
        let mut frames = Vec::new();
        for _ in 0..2 {
            match self.decoder.send_packet(&packet) {
                Ok(()) => {
                    frames.extend(self.receive()?);
                    return Ok(frames);
                }
                Err(ffmpeg::Error::Other { errno }) if errno == ffmpeg::error::EAGAIN => {
                    // A decoder may ask the caller to drain output before it
                    // accepts the next packet. EAGAIN is normal flow control;
                    // only a second refusal after draining is a real error.
                    frames.extend(self.receive()?);
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("decoder input remained back-pressured after draining")
    }
    fn receive(&mut self) -> Result<Vec<VideoFrame>> {
        let mut frames = Vec::new();
        let mut decoded = frame::Video::empty();
        loop {
            if let Err(error) = self.decoder.receive_frame(&mut decoded) {
                if matches!(error, ffmpeg::Error::Eof)
                    || matches!(error, ffmpeg::Error::Other { errno } if errno == ffmpeg::error::EAGAIN)
                {
                    break;
                }
                return Err(error.into());
            }
            let size = (decoded.width(), decoded.height());
            if size.0 == 0
                || size.1 == 0
                || !size.0.is_multiple_of(2)
                || !size.1.is_multiple_of(2)
                || size.0 > i32::MAX as u32
                || size.1 > i32::MAX as u32
                || size.0 > MAX_VIDEO_DIMENSION
                || size.1 > MAX_VIDEO_DIMENSION
            {
                bail!("decoder returned invalid YUV420 dimensions")
            }
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
            let cw = size.0.div_ceil(2);
            let ch = size.1.div_ceil(2);
            let y_size = (size.0 as usize)
                .checked_mul(size.1 as usize)
                .context("decoded frame dimensions overflow")?;
            let chroma_size = (cw as usize)
                .checked_mul(ch as usize)
                .and_then(|value| value.checked_mul(2))
                .context("decoded frame dimensions overflow")?;
            let mut packed = Vec::with_capacity(
                y_size
                    .checked_add(chroma_size)
                    .context("decoded frame dimensions overflow")?,
            );
            copy_plane(source, 0, size.0, size.1, &mut packed)?;
            copy_plane(source, 1, cw, ch, &mut packed)?;
            copy_plane(source, 2, cw, ch, &mut packed)?;
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
fn copy_plane(
    frame: &frame::Video,
    index: usize,
    width: u32,
    height: u32,
    output: &mut Vec<u8>,
) -> Result<()> {
    let stride = frame.stride(index);
    let data = frame.data(index);
    if stride < width as usize {
        bail!("decoder plane stride is smaller than its width")
    }
    let required = height
        .checked_sub(1)
        .and_then(|rows| (rows as usize).checked_mul(stride))
        .and_then(|start| start.checked_add(width as usize))
        .context("decoder plane dimensions overflow")?;
    if data.len() < required {
        bail!("decoder plane buffer is shorter than its stride")
    }
    for row in 0..height as usize {
        let start = row * stride;
        output.extend_from_slice(&data[start..start + width as usize]);
    }
    Ok(())
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

    #[test]
    fn luma_signature_detects_local_tile_changes() {
        let mut baseline = vec![16u8; 32 * 18 * 3 / 2];
        let first = VideoFrame::new_i420(32, 18, baseline.clone(), 0)
            .luma_signature()
            .unwrap();
        for row in 6..12 {
            for column in 10..16 {
                baseline[row * 32 + column] = 220;
            }
        }
        let changed = VideoFrame::new_i420(32, 18, baseline, 0)
            .luma_signature()
            .unwrap();
        assert_ne!(first, changed);
    }
}
