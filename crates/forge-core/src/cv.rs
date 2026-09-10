use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::{bail, Context, Result};
use image::RgbImage;

use crate::video::VideoFrame;

const CACHE_CAPACITY: usize = 32;
const FIND_ALL_CAPACITY: usize = 4096;
const AUTO_SCALES: &[(u32, u32)] = &[(1, 1), (3, 2), (2, 1), (5, 2)];
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
        best_only: bool,
        priority_first: bool,
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
}
impl GrayTemplateCache {
    fn load(&mut self, path: &Path) -> Result<Arc<image::GrayImage>> {
        let metadata = std::fs::metadata(path)?;
        let stamp = (
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|v| v.as_nanos())
                .unwrap_or(0),
        );
        if self.stamps.get(path) == Some(&stamp) {
            if let Some(value) = self.images.get(path).cloned() {
                self.touch(path);
                return Ok(value);
            }
        }
        let image = Arc::new(
            image::open(path)
                .with_context(|| format!("unable to load template {}", path.display()))?
                .into_luma8(),
        );
        let already_cached = self.images.contains_key(path);
        self.order.retain(|item| item != path);
        if !already_cached && self.images.len() >= CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.images.remove(&old);
                self.stamps.remove(&old);
            }
        }
        self.images.insert(path.to_owned(), image.clone());
        self.stamps.insert(path.to_owned(), stamp);
        self.order.push_back(path.to_owned());
        Ok(image)
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
}

impl TemplateCache {
    fn load_pyramid(&mut self, path: &Path, multiscale: bool) -> Result<Arc<Vec<RgbImage>>> {
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("unable to stat template {}", path.display()))?;
        let stamp = (
            metadata.len(),
            metadata
                .modified()
                .ok()
                .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|v| v.as_nanos())
                .unwrap_or(0),
        );
        let key = (path.to_owned(), multiscale);
        if self.stamps.get(&key) == Some(&stamp) {
            if let Some(value) = self.images.get(&key).cloned() {
                self.touch(&key);
                return Ok(value);
            }
        }
        let original = image::open(path)
            .with_context(|| format!("unable to load template {}", path.display()))?
            .into_rgb8();
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
                        original.clone()
                    } else {
                        image::imageops::resize(
                            &original,
                            original.width() * num / den,
                            original.height() * num / den,
                            image::imageops::FilterType::Triangle,
                        )
                    }
                })
                .collect::<Vec<_>>(),
        );
        let already_cached = self.images.contains_key(&key);
        self.order.retain(|item| item != &key);
        if !already_cached && self.images.len() >= CACHE_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.images.remove(&old);
                self.stamps.remove(&old);
            }
        }
        self.images.insert(key.clone(), pyramid.clone());
        self.stamps.insert(key.clone(), stamp);
        self.order.push_back(key);
        Ok(pyramid)
    }

    fn touch(&mut self, key: &(PathBuf, bool)) {
        if let Some(index) = self.order.iter().position(|item| item == key) {
            self.order.remove(index);
        }
        self.order.push_back(key.clone());
    }
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
    let pyramid = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .load_pyramid(path, false)?;
    let image = &pyramid[0];
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
    let image = GRAY_CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .load(path)?;
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
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    let pyramids = paths
        .iter()
        .map(|path| cache.load_pyramid(path, false))
        .collect::<Result<Vec<_>>>()?;
    drop(cache);
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
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    let pyramids = paths
        .iter()
        .map(|path| cache.load_pyramid(path, false))
        .collect::<Result<Vec<_>>>()?;
    drop(cache);
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
    let pyramid = CACHE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .load_pyramid(path, multiscale)?;
    let selected = if multiscale {
        &pyramid[..]
    } else {
        &pyramid[..1]
    };
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
            (x2 - x1) * 3,
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
        frame.width * 3,
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
    if !threshold.is_finite() || !(-1.0..=1.0).contains(&threshold) {
        bail!("template threshold must be finite and between -1 and 1");
    }
    if native_templates.is_empty() {
        return Ok(Vec::new());
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
            best_only,
            priority_first,
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
        let mut cache = TemplateCache::default();
        assert_eq!(cache.load_pyramid(&path, false).unwrap().len(), 1);
        assert_eq!(
            cache.load_pyramid(&path, true).unwrap().len(),
            AUTO_SCALES.len()
        );
        let second = image::RgbImage::from_pixel(5, 4, image::Rgb([0, 255, 0]));
        second.save(&path).unwrap();
        let loaded = cache.load_pyramid(&path, false).unwrap();
        assert_eq!(loaded[0].width(), 5);
        assert_eq!(loaded[0].get_pixel(0, 0).0, [0, 255, 0]);
        let _ = std::fs::remove_file(path);
    }
}
