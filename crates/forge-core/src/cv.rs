use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{bail, Context, Result};
use image::{ImageReader, Limits, RgbImage};

use crate::video::VideoFrame;

// Gray and RGB pyramids use separate locks and pools. Keeping each pool at
// 64 MiB bounds their combined resident template memory at 128 MiB while
// allowing either representation to evict independently.
const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const FIND_ALL_CAPACITY: usize = 4096;
const AUTO_SCALES: &[(u32, u32)] = &[(1, 1), (3, 2), (2, 1), (5, 2)];
const MAX_TEMPLATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEMPLATE_DIMENSION: u32 = 8192;
const MAX_TEMPLATES_PER_MATCH: usize = 64;
static CACHE: OnceLock<Mutex<TemplateCache>> = OnceLock::new();
static GRAY_CACHE: OnceLock<Mutex<GrayTemplateCache>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct Match {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub confidence: f32,
    pub template_index: usize,
}

#[repr(C)]
struct NativeTemplate {
    data: *const u8,
    width: i32,
    height: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NativeMatch {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    confidence: f32,
    template_index: i32,
}

extern "C" {
    fn forge_cv_set_threads(threads: i32);
    fn forge_match_template_rgb(
        rgb: *const u8,
        width: i32,
        height: i32,
        stride: i32,
        channels: i32,
        templates: *const NativeTemplate,
        template_count: i32,
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        threshold: f32,
        best_only: u8,
        priority_first: u8,
        coarse_candidates: i32,
        output: *mut NativeMatch,
        capacity: i32,
    ) -> i32;
}

#[derive(Default)]
struct GrayTemplateCache {
    images: HashMap<PathBuf, Arc<image::GrayImage>>,
    stamps: HashMap<PathBuf, (u64, u128)>,
    order: VecDeque<PathBuf>,
    bytes: usize,
}
impl GrayTemplateCache {
    fn lookup(&mut self, path: &Path, stamp: (u64, u128)) -> Option<Arc<image::GrayImage>> {
        if self.stamps.get(path) == Some(&stamp) {
            if let Some(value) = self.images.get(path).cloned() {
                self.touch(path);
                return Some(value);
            }
        }
        None
    }

    fn insert(&mut self, path: PathBuf, stamp: (u64, u128), image: Arc<image::GrayImage>) {
        let size = image.as_raw().len();
        if let Some(previous) = self.images.remove(&path) {
            self.bytes = self.bytes.saturating_sub(previous.as_raw().len());
        }
        self.stamps.remove(&path);
        self.order.retain(|item| item != &path);
        if size > MAX_CACHE_BYTES {
            return;
        }
        while self.bytes.saturating_add(size) > MAX_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.images.remove(&old) {
                self.bytes = self.bytes.saturating_sub(value.as_raw().len());
            }
            self.stamps.remove(&old);
        }
        self.bytes = self.bytes.saturating_add(size);
        self.images.insert(path.clone(), image);
        self.stamps.insert(path.clone(), stamp);
        self.order.push_back(path);
    }

    fn touch(&mut self, path: &Path) {
        if let Some(index) = self.order.iter().position(|item| item == path) {
            self.order.remove(index);
        }
        self.order.push_back(path.to_owned());
    }
}

#[derive(Default)]
struct TemplateCache {
    // A single-scale lookup must not pay for the resize pyramid, while a
    // later multiscale lookup still needs its own cached representation.
    // Keeping the mode in the key avoids returning a one-image cache entry to
    // a multiscale caller.
    images: HashMap<(PathBuf, bool), Arc<Vec<RgbImage>>>,
    stamps: HashMap<(PathBuf, bool), (u64, u128)>,
    order: VecDeque<(PathBuf, bool)>,
    bytes: usize,
}

impl TemplateCache {
    fn lookup(&mut self, key: &(PathBuf, bool), stamp: (u64, u128)) -> Option<Arc<Vec<RgbImage>>> {
        if self.stamps.get(key) == Some(&stamp) {
            if let Some(value) = self.images.get(key).cloned() {
                self.touch(key);
                return Some(value);
            }
        }
        None
    }

    fn insert(&mut self, key: (PathBuf, bool), stamp: (u64, u128), pyramid: Arc<Vec<RgbImage>>) {
        let size = pyramid
            .iter()
            .map(|image| image.as_raw().len())
            .sum::<usize>();
        if let Some(previous) = self.images.remove(&key) {
            let old_size = previous
                .iter()
                .map(|image| image.as_raw().len())
                .sum::<usize>();
            self.bytes = self.bytes.saturating_sub(old_size);
        }
        self.stamps.remove(&key);
        self.order.retain(|item| item != &key);
        if size > MAX_CACHE_BYTES {
            return;
        }
        while self.bytes.saturating_add(size) > MAX_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(value) = self.images.remove(&old) {
                let old_size = value
                    .iter()
                    .map(|image| image.as_raw().len())
                    .sum::<usize>();
                self.bytes = self.bytes.saturating_sub(old_size);
            }
            self.stamps.remove(&old);
        }
        self.bytes = self.bytes.saturating_add(size);
        self.images.insert(key.clone(), pyramid);
        self.stamps.insert(key.clone(), stamp);
        self.order.push_back(key);
    }

    fn touch(&mut self, key: &(PathBuf, bool)) {
        if let Some(index) = self.order.iter().position(|item| item == key) {
            self.order.remove(index);
        }
        self.order.push_back(key.clone());
    }
}

fn template_stamp(path: &Path) -> Result<(PathBuf, (u64, u128))> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let metadata = std::fs::metadata(&key)
        .with_context(|| format!("unable to stat template {}", path.display()))?;
    let stamp = (
        metadata.len(),
        metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or(0),
    );
    Ok((key, stamp))
}

fn load_template_image(path: &Path) -> Result<image::DynamicImage> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("unable to stat template {}", path.display()))?;
    if metadata.len() > MAX_TEMPLATE_BYTES as u64 {
        bail!("template file is too large")
    }
    let mut reader = ImageReader::open(path)
        .with_context(|| format!("unable to open template {}", path.display()))?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_TEMPLATE_DIMENSION);
    limits.max_image_height = Some(MAX_TEMPLATE_DIMENSION);
    limits.max_alloc = Some((MAX_TEMPLATE_BYTES as u64).saturating_mul(4));
    reader.limits(limits);
    reader
        .decode()
        .with_context(|| format!("unable to decode template {}", path.display()))
}

fn load_gray_template(path: &Path) -> Result<Arc<image::GrayImage>> {
    let (key, stamp) = template_stamp(path)?;
    if let Some(value) = GRAY_CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .lookup(&key, stamp)
    {
        return Ok(value);
    }
    // Decode outside the global cache lock. Different devices/scripts should
    // never block each other on file I/O or image conversion.
    let image = Arc::new(load_template_image(&key)?.into_luma8());
    let mut cache = GRAY_CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(value) = cache.lookup(&key, stamp) {
        return Ok(value);
    }
    cache.insert(key, stamp, image.clone());
    Ok(image)
}

fn load_rgb_pyramid(path: &Path, multiscale: bool) -> Result<Arc<Vec<RgbImage>>> {
    let (path, stamp) = template_stamp(path)?;
    let key = (path.clone(), multiscale);
    if let Some(value) = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .lookup(&key, stamp)
    {
        return Ok(value);
    }
    // Decode and resize outside the global cache lock for multi-device
    // parallelism. A second lookup below coalesces a concurrent duplicate.
    let original = load_template_image(&path)?.into_rgb8();
    let scales = if multiscale {
        AUTO_SCALES
    } else {
        &AUTO_SCALES[..1]
    };
    let pyramid = Arc::new(
        scales
            .iter()
            .map(|&(num, den)| {
                if num == den {
                    return Ok(original.clone());
                }
                let width = original
                    .width()
                    .checked_mul(num)
                    .and_then(|value| value.checked_div(den))
                    .context("scaled template width overflow")?;
                let height = original
                    .height()
                    .checked_mul(num)
                    .and_then(|value| value.checked_div(den))
                    .context("scaled template height overflow")?;
                Ok(image::imageops::resize(
                    &original,
                    width.max(1),
                    height.max(1),
                    image::imageops::FilterType::Triangle,
                ))
            })
            .collect::<Result<Vec<_>>>()?,
    );
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(value) = cache.lookup(&key, stamp) {
        return Ok(value);
    }
    cache.insert(key, stamp, pyramid.clone());
    Ok(pyramid)
}

pub fn find(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Option<Match>> {
    Ok(find_inner(frame, path, threshold, roi, true, false)?
        .into_iter()
        .next())
}

/// Latency-first single-scale search. It refines only the two strongest coarse
/// peaks; `find` remains the safer eight-candidate path for difficult images.
pub fn find_fast(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Option<Match>> {
    let pyramid = load_rgb_pyramid(path, false)?;
    let image = &pyramid[0];
    validate_rgb_template(image)?;
    let native = [NativeTemplate {
        data: image.as_raw().as_ptr(),
        width: image.width() as i32,
        height: image.height() as i32,
    }];
    Ok(run_native(frame, &native, threshold, roi, true, false, 2)?
        .into_iter()
        .next())
}

pub fn find_gray(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
    fast: bool,
) -> Result<Option<Match>> {
    let image = load_gray_template(path)?;
    validate_gray_template(&image)?;
    let native = [NativeTemplate {
        data: image.as_raw().as_ptr(),
        width: image.width() as i32,
        height: image.height() as i32,
    }];
    Ok(run_native_data(
        frame.y_plane_checked()?,
        frame.width,
        frame.height,
        frame.width,
        &native,
        threshold,
        roi,
        true,
        false,
        if fast { 2 } else { 8 },
        1,
    )?
    .into_iter()
    .next())
}

pub fn find_multiscale(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Option<Match>> {
    Ok(find_inner(frame, path, threshold, roi, true, true)?
        .into_iter()
        .next())
}

pub fn find_all(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Vec<Match>> {
    find_inner(frame, path, threshold, roi, false, false)
}

/// 按模板顺序返回第一个命中项，整帧颜色转换只执行一次。
pub fn find_first(
    frame: &VideoFrame,
    paths: &[PathBuf],
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Option<(usize, Match)>> {
    if paths.is_empty() {
        return Ok(None);
    }
    if paths.len() > MAX_TEMPLATES_PER_MATCH {
        bail!("a match request may contain at most {MAX_TEMPLATES_PER_MATCH} templates");
    }
    let pyramids = paths
        .iter()
        .map(|path| load_rgb_pyramid(path, false))
        .collect::<Result<Vec<_>>>()?;
    for pyramid in &pyramids {
        validate_rgb_template(&pyramid[0])?;
    }
    let native = pyramids
        .iter()
        .map(|pyramid| {
            let image = &pyramid[0];
            NativeTemplate {
                data: image.as_raw().as_ptr(),
                width: image.width() as i32,
                height: image.height() as i32,
            }
        })
        .collect::<Vec<_>>();
    Ok(run_native(frame, &native, threshold, roi, true, true, 8)?
        .into_iter()
        .next()
        .map(|m| (m.template_index, m)))
}

/// Return the best candidate for every template that reaches `threshold`.
/// Unlike `find_first`, this never lets an earlier template hide a stronger
/// match and is the native primitive used by the per-target tracker.
pub fn find_candidates(
    frame: &VideoFrame,
    paths: &[PathBuf],
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
) -> Result<Vec<Match>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if paths.len() > MAX_TEMPLATES_PER_MATCH {
        bail!("a match request may contain at most {MAX_TEMPLATES_PER_MATCH} templates");
    }
    let pyramids = paths
        .iter()
        .map(|path| load_rgb_pyramid(path, false))
        .collect::<Result<Vec<_>>>()?;
    for pyramid in &pyramids {
        validate_rgb_template(&pyramid[0])?;
    }
    let native = pyramids
        .iter()
        .map(|pyramid| {
            let image = &pyramid[0];
            NativeTemplate {
                data: image.as_raw().as_ptr(),
                width: image.width() as i32,
                height: image.height() as i32,
            }
        })
        .collect::<Vec<_>>();
    run_native(frame, &native, threshold, roi, true, false, 4)
}

fn find_inner(
    frame: &VideoFrame,
    path: &Path,
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
    best_only: bool,
    multiscale: bool,
) -> Result<Vec<Match>> {
    let pyramid = load_rgb_pyramid(path, multiscale)?;
    let selected = if multiscale {
        &pyramid[..]
    } else {
        &pyramid[..1]
    };
    for image in selected {
        validate_rgb_template(image)?;
    }
    let native_templates = selected
        .iter()
        .map(|image| NativeTemplate {
            data: image.as_raw().as_ptr(),
            width: image.width() as i32,
            height: image.height() as i32,
        })
        .collect::<Vec<_>>();
    run_native(
        frame,
        &native_templates,
        threshold,
        roi,
        best_only,
        false,
        8,
    )
}

fn run_native(
    frame: &VideoFrame,
    native_templates: &[NativeTemplate],
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
    best_only: bool,
    priority_first: bool,
    coarse_candidates: i32,
) -> Result<Vec<Match>> {
    if let Some(requested_roi) = roi {
        let (rgb, (x1, y1, x2, y2)) = frame.rgb_roi(requested_roi)?;
        let mut found = run_native_data(
            &rgb,
            x2 - x1,
            y2 - y1,
            (x2 - x1)
                .checked_mul(3)
                .context("RGB ROI stride overflow")?,
            native_templates,
            threshold,
            None,
            best_only,
            priority_first,
            coarse_candidates,
            3,
        )?;
        for item in &mut found {
            item.x += x1 as i32;
            item.y += y1 as i32;
        }
        return Ok(found);
    }
    run_native_data(
        frame.rgb()?,
        frame.width,
        frame.height,
        frame.width.checked_mul(3).context("RGB stride overflow")?,
        native_templates,
        threshold,
        roi,
        best_only,
        priority_first,
        coarse_candidates,
        3,
    )
}
#[allow(clippy::too_many_arguments)]
fn run_native_data(
    data: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    native_templates: &[NativeTemplate],
    threshold: f32,
    roi: Option<(u32, u32, u32, u32)>,
    best_only: bool,
    priority_first: bool,
    coarse_candidates: i32,
    channels: i32,
) -> Result<Vec<Match>> {
    if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
        bail!("template threshold must be finite and between 0 and 1");
    }
    if native_templates.is_empty() {
        return Ok(Vec::new());
    }
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        bail!("invalid frame dimensions");
    }
    if channels != 1 && channels != 3 {
        bail!("unsupported image channel count");
    }
    let min_stride = (width as usize)
        .checked_mul(channels as usize)
        .context("image stride overflow")?;
    if stride < min_stride as u32 || stride > i32::MAX as u32 {
        bail!("image stride is smaller than the row width");
    }
    let required = (height as usize)
        .checked_mul(stride as usize)
        .context("image dimensions overflow")?;
    if data.len() < required {
        bail!("image buffer is shorter than its declared stride");
    }
    if native_templates.len() > i32::MAX as usize {
        bail!("too many templates");
    }
    static CONFIGURED: OnceLock<()> = OnceLock::new();
    CONFIGURED.get_or_init(|| {
        let default = std::thread::available_parallelism()
            .map(|v| v.get().min(4))
            .unwrap_or(2);
        let threads = std::env::var("SCRCPYFORGE_CV_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(default)
            .clamp(1, 32);
        unsafe { forge_cv_set_threads(threads as i32) }
    });
    let (x1, y1, x2, y2) = roi.unwrap_or((0, 0, width, height));
    if x1 >= x2 || y1 >= y2 || x2 > width || y2 > height {
        bail!("invalid ROI");
    }
    let capacity = if best_only {
        // `best_only` means one result per template, not one result for the
        // entire template set. Keeping this bounded avoids the old 4096-slot
        // allocation while preserving every target's strongest candidate.
        native_templates.len()
    } else {
        FIND_ALL_CAPACITY
    };
    let mut output = vec![NativeMatch::default(); capacity];
    let count = unsafe {
        forge_match_template_rgb(
            data.as_ptr(),
            width as i32,
            height as i32,
            stride as i32,
            channels,
            native_templates.as_ptr(),
            native_templates.len() as i32,
            x1 as i32,
            y1 as i32,
            x2 as i32,
            y2 as i32,
            threshold,
            best_only as u8,
            priority_first as u8,
            coarse_candidates,
            output.as_mut_ptr(),
            capacity as i32,
        )
    };
    if count < 0 {
        bail!("OpenCV template matching failed with code {count}");
    }
    output.truncate((count as usize).min(capacity));
    Ok(output
        .into_iter()
        .map(|item| Match {
            x: item.x,
            y: item.y,
            width: item.width as u32,
            height: item.height as u32,
            confidence: item.confidence.clamp(0.0, 1.0),
            template_index: item.template_index.max(0) as usize,
        })
        .collect())
}

fn validate_rgb_template(image: &RgbImage) -> Result<()> {
    if image.width() == 0
        || image.height() == 0
        || image.width() > i32::MAX as u32
        || image.height() > i32::MAX as u32
    {
        bail!("template dimensions are invalid")
    }
    let bytes = (image.width() as usize)
        .checked_mul(image.height() as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .context("template dimensions overflow")?;
    if bytes == 0 || bytes > MAX_TEMPLATE_BYTES {
        bail!("template is empty or too large")
    }
    let (min, max) = image
        .as_raw()
        .iter()
        .fold((u8::MAX, u8::MIN), |(min, max), value| {
            (min.min(*value), max.max(*value))
        });
    if max.saturating_sub(min) < 2 {
        bail!("template has insufficient contrast")
    }
    Ok(())
}

fn validate_gray_template(image: &image::GrayImage) -> Result<()> {
    if image.width() == 0
        || image.height() == 0
        || image.width() > i32::MAX as u32
        || image.height() > i32::MAX as u32
    {
        bail!("template dimensions are invalid")
    }
    let bytes = (image.width() as usize)
        .checked_mul(image.height() as usize)
        .context("template dimensions overflow")?;
    if bytes == 0 || bytes > MAX_TEMPLATE_BYTES {
        bail!("template is empty or too large")
    }
    let (min, max) = image
        .as_raw()
        .iter()
        .fold((u8::MAX, u8::MIN), |(min, max), value| {
            (min.min(*value), max.max(*value))
        });
    if max.saturating_sub(min) < 2 {
        bail!("template has insufficient contrast")
    }
    Ok(())
}

pub fn save(frame: &VideoFrame, path: &Path) -> Result<()> {
    image::save_buffer(
        path,
        frame.rgb()?,
        frame.width,
        frame.height,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(())
}

pub fn crop(frame: &VideoFrame, path: &Path, roi: (u32, u32, u32, u32)) -> Result<()> {
    let (x1, y1, x2, y2) = roi;
    if x1 >= x2 || y1 >= y2 || x2 > frame.width || y2 > frame.height {
        bail!("invalid crop");
    }
    let image = image::RgbImage::from_raw(frame.width, frame.height, frame.rgb()?.to_vec())
        .context("invalid RGB frame")?;
    image::imageops::crop_imm(&image, x1, y1, x2 - x1, y2 - y1)
        .to_image()
        .save(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overwritten_template_invalidates_cache() {
        let path = std::env::temp_dir().join(format!("forge-cache-{}.png", std::process::id()));
        let first = image::RgbImage::from_pixel(4, 4, image::Rgb([255, 0, 0]));
        first.save(&path).unwrap();
        assert_eq!(load_rgb_pyramid(&path, false).unwrap().len(), 1);
        assert_eq!(
            load_rgb_pyramid(&path, true).unwrap().len(),
            AUTO_SCALES.len()
        );
        let second = image::RgbImage::from_pixel(5, 4, image::Rgb([0, 255, 0]));
        second.save(&path).unwrap();
        let loaded = load_rgb_pyramid(&path, false).unwrap();
        assert_eq!(loaded[0].width(), 5);
        assert_eq!(loaded[0].get_pixel(0, 0).0, [0, 255, 0]);
        let _ = std::fs::remove_file(path);
    }
}
