//! 闭眼检测：专治「合影里总有人眨眼」。
//!
//! 为什么值得单独做：一张废片里，闭眼是最难一眼看出来、又最没法救的一种。
//! 清晰度、曝光都已经是现成的画面指标，但它们看不出眼睛是睁是闭——
//! 眼睛那块像素只有几十个，糊和闭在统计上长得一模一样。所以得先找到脸。
//!
//! 做法分两步，模型都在本地跑（rten，纯 Rust，不联网、不调任何 API）：
//!
//! 1. **YuNet**（OpenCV Zoo，Apache-2.0，232KB）找出画面里的人脸框；
//! 2. **MediaPipe FaceMesh**（256×256，478 点）在每张脸里定位眼眶，
//!    用**眼睛纵横比 EAR**（Eye Aspect Ratio）判断睁闭。
//!
//! EAR 是「眼睛高度 ÷ 眼睛宽度」：睁着眼大约 0.25–0.35，闭眼会掉到 0.15 以下。
//! 它是比值，所以不受人脸大小、镜头焦段影响——这正是我们想要的：
//! 合影后排的小脸和特写的大脸用同一个门槛。
//!
//! 检测只在开关打开时跑（见 `blink_enabled`）：默认关，因为要解码、要推理，
//! 几千张不是免费的，不想让不需要它的人白等。

use anyhow::{anyhow, Context, Result};
use image::imageops::FilterType;
use image::RgbImage;
use rten::Model;
use rten_tensor::prelude::*;
use rten_tensor::{NdTensor, Tensor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::thumb;

/// 人脸检测的输入边长。YuNet 训练时定死 640，换别的尺寸要连模型一起换。
const DET_SIZE: usize = 640;
/// 关键点模型的输入边长。
const LMK_SIZE: usize = 256;
/// 低于这个置信度的人脸不认。宁可漏，也不要把花瓶当成脸去算眼睛。
const FACE_SCORE: f32 = 0.6;
/// 两个框重合度超过这个值，只留分数高的那个。
const NMS_IOU: f32 = 0.3;
/// 一张图最多算多少张脸。合影再多，选片时也只看得清前排这几张。
const MAX_FACES: usize = 12;
/// 检测用的工作尺寸上限。原片动辄五六千像素宽，缩到这个尺寸脸还是清楚的，
/// 但解码和缩放快得多。
const WORK_SIZE: u32 = 1280;

/// EAR 低于这个值判为闭眼。
///
/// 经验值，不是理论推导：睁眼普遍在 0.25 以上，闭眼掉到 0.15 以下，
/// 门槛卡在中间偏下（0.21），宁可漏掉半睁的，也不把好片标成闭眼——
/// 被误标的代价（重新看一遍）比漏标高。
pub const CLOSED_THRESHOLD: f64 = 0.21;

/// 一张照片的眼睛检查结果。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EyeReport {
    /// 检出的人脸数。0 = 这张照片里没找到脸（风景、静物都算这一类）。
    pub faces: usize,
    /// 全图最「闭」的那只眼睛的 EAR。None = 一张脸都没测出关键点。
    pub ratio: Option<f64>,
}

impl EyeReport {
    /// 是否疑似闭眼。
    pub fn closed(&self) -> bool {
        matches!(self.ratio, Some(r) if r < CLOSED_THRESHOLD)
    }
}

// ── 模型加载 ────────────────────────────────────────────────────────────

struct Engine {
    det: Model,
    lmk: Model,
}

/// 模型目录：打包后由 Tauri 的资源目录给出，开发时退回仓库里的那一份。
static MODEL_DIR: OnceLock<PathBuf> = OnceLock::new();
/// 模型只在第一次用到时才加载，而且只加载一次。
static ENGINE: OnceLock<Result<Arc<Engine>, String>> = OnceLock::new();

/// 记下资源目录（应用启动时调一次）。找不到模型也不该让应用起不来，
/// 所以这里不返回结果。
pub fn set_model_dir(dir: PathBuf) {
    let _ = MODEL_DIR.set(dir);
}

/// 依次试几个候选位置，取第一个同时放了两个模型的。
fn model_dir() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(d) = MODEL_DIR.get() {
        candidates.push(d.join("models"));
        candidates.push(d.clone());
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources/models"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            candidates.push(p.join("models"));
            candidates.push(p.join("resources/models"));
        }
    }
    candidates
        .into_iter()
        .find(|d| d.join("yunet.onnx").exists() && d.join("face_landmark.onnx").exists())
}

fn engine() -> Result<Arc<Engine>, String> {
    ENGINE
        .get_or_init(|| {
            let dir = model_dir().ok_or_else(|| "找不到闭眼检测用的模型文件".to_string())?;
            let det = Model::load_file(dir.join("yunet.onnx"))
                .map_err(|e| format!("人脸检测模型加载失败：{e}"))?;
            let lmk = Model::load_file(dir.join("face_landmark.onnx"))
                .map_err(|e| format!("人脸关键点模型加载失败：{e}"))?;
            Ok(Arc::new(Engine { det, lmk }))
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|e| e.clone())
}

// ── 取图 ────────────────────────────────────────────────────────────────

/// 取一张够看的 RGB 图。
///
/// 优先用 RAW 里内嵌的 JPEG（快一个数量级）；内嵌预览太小时才退到完整解码——
/// 一百多像素宽的脸，神仙也算不出眼眶在哪。
fn load_rgb(path: &Path) -> Result<RgbImage> {
    if let Ok((bytes, _)) = thumb::best_jpeg_from_file(path) {
        if let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg) {
            let img = img.into_rgb8();
            if img.width() >= 320 && img.height() >= 320 {
                return Ok(shrink(img));
            }
        }
    }
    let (img, _) = thumb::decode_source(path)?;
    Ok(shrink(img.into_rgb8()))
}

/// 缩到工作尺寸。只缩不放：本来就小的图（老照片、裁切图）保持原样，
/// 放大只会让模型看到插值出来的假边缘。
fn shrink(img: RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    let long = w.max(h);
    if long <= WORK_SIZE {
        return img;
    }
    let scale = WORK_SIZE as f32 / long as f32;
    image::imageops::resize(
        &img,
        (w as f32 * scale).round().max(1.0) as u32,
        (h as f32 * scale).round().max(1.0) as u32,
        FilterType::Triangle,
    )
}

// ── 人脸检测 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct FaceBox {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    score: f32,
}

/// 检测图的尺寸和它在 640×640 画布里的位置——检测在画布坐标里做，
/// 拿到框以后要按这个换回原图坐标。
struct Letterbox {
    scale: f32,
    off_x: f32,
    off_y: f32,
}

/// 等比缩放后居中贴进正方形，四周补黑。
///
/// 为什么不直接拉成 640×640：3:2 的照片硬拉成方的，脸会被压扁 1.5 倍，
/// 检测器的命中率明显下降。补黑边牺牲一点有效像素，换的是脸不变形。
fn letterbox(img: &RgbImage, size: usize) -> (RgbImage, Letterbox) {
    let (w, h) = img.dimensions();
    let scale = size as f32 / w.max(h) as f32;
    let nw = ((w as f32 * scale).round() as u32).max(1).min(size as u32);
    let nh = ((h as f32 * scale).round() as u32).max(1).min(size as u32);
    let small = image::imageops::resize(img, nw, nh, FilterType::Triangle);
    let mut canvas = RgbImage::from_pixel(size as u32, size as u32, image::Rgb([0, 0, 0]));
    let off_x = ((size as u32 - nw) / 2) as i64;
    let off_y = ((size as u32 - nh) / 2) as i64;
    image::imageops::overlay(&mut canvas, &small, off_x, off_y);
    (
        canvas,
        Letterbox {
            scale,
            off_x: off_x as f32,
            off_y: off_y as f32,
        },
    )
}

/// YuNet 的输出解码（对齐 OpenCV `FaceDetectorYN` 的实现）。
///
/// 三个尺度的特征图，每个格子一个人脸候选：
/// 置信度是分类分和「这里有一张脸」分的几何平均，框是中心点 + 指数尺寸，
/// 关键点是相对格子的偏移。这套解码写错一个符号，表现是「一张脸也找不到」——
/// 所以下面这几个常量和 OpenCV 的源码一一对应，不要凭感觉改。
fn detect_faces(e: &Engine, img: &RgbImage) -> Result<Vec<FaceBox>> {
    let (canvas, lb) = letterbox(img, DET_SIZE);

    // OpenCV 的 demo 直接把 BGR 原图（0–255）喂进去，这里保持同一口径
    let mut input = Tensor::<f32>::zeros(&[1, 3, DET_SIZE, DET_SIZE]);
    for y in 0..DET_SIZE {
        for x in 0..DET_SIZE {
            let p = canvas.get_pixel(x as u32, y as u32);
            input[[0, 0, y, x]] = p[2] as f32;
            input[[0, 1, y, x]] = p[1] as f32;
            input[[0, 2, y, x]] = p[0] as f32;
        }
    }

    // 关键点输出（kps_*）没要：我们只用检测框，YuNet 附带的五个点用不上，
    // 少要三个输出就少拷三份数据。
    let names = [
        "cls_8", "cls_16", "cls_32", "obj_8", "obj_16", "obj_32", "bbox_8", "bbox_16", "bbox_32",
    ];
    let ids = names
        .iter()
        .map(|n| e.det.node_id(n).map_err(|e| anyhow!("模型缺输出 {n}：{e}")))
        .collect::<Result<Vec<_>>>()?;

    let outs = e
        .det
        .run(
            vec![(
                e.det.node_id("input").map_err(|e| anyhow!("{e}"))?,
                input.view().into(),
            )],
            &ids,
            None,
        )
        .map_err(|e| anyhow!("人脸检测推理失败：{e}"))?;
    let tensors: Vec<NdTensor<f32, 3>> = outs
        .into_iter()
        .map(|v| v.try_into().map_err(|e| anyhow!("模型输出类型不对：{e:?}")))
        .collect::<Result<Vec<_>>>()?;

    let mut boxes = Vec::new();
    for (i, stride) in [8usize, 16, 32].iter().enumerate() {
        let cls = &tensors[i];
        let obj = &tensors[i + 3];
        let bbox = &tensors[i + 6];

        let cols = DET_SIZE / stride;
        let rows = DET_SIZE / stride;
        for r in 0..rows {
            for c in 0..cols {
                let cls_score = cls[[0, r * cols + c, 0]].clamp(0.0, 1.0);
                let obj_score = obj[[0, r * cols + c, 0]].clamp(0.0, 1.0);
                let score = (cls_score * obj_score).sqrt();
                if score < FACE_SCORE {
                    continue;
                }
                let cx = (c as f32 + bbox[[0, r * cols + c, 0]]) * *stride as f32;
                let cy = (r as f32 + bbox[[0, r * cols + c, 1]]) * *stride as f32;
                let w = bbox[[0, r * cols + c, 2]].exp() * *stride as f32;
                let h = bbox[[0, r * cols + c, 3]].exp() * *stride as f32;

                boxes.push(FaceBox {
                    x: (cx - w / 2.0 - lb.off_x) / lb.scale,
                    y: (cy - h / 2.0 - lb.off_y) / lb.scale,
                    w: w / lb.scale,
                    h: h / lb.scale,
                    score,
                });
            }
        }
    }

    Ok(nms(boxes))
}

/// 非极大值抑制：同一张脸会被相邻格子重复检出一堆框，按分数从高到低留，
/// 和已留下的框重叠太多的丢掉。
fn nms(mut boxes: Vec<FaceBox>) -> Vec<FaceBox> {
    boxes.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut kept: Vec<FaceBox> = Vec::new();
    for b in boxes {
        if kept.iter().any(|k| iou(k, &b) > NMS_IOU) {
            continue;
        }
        kept.push(b);
        if kept.len() >= MAX_FACES {
            break;
        }
    }
    kept
}

fn iou(a: &FaceBox, b: &FaceBox) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    let inter = (x1 - x0).max(0.0) * (y1 - y0).max(0.0);
    let union = a.w * a.h + b.w * b.h - inter;
    if union <= 0.0 {
        0.0
    } else {
        inter / union
    }
}

// ── 眼睛开合 ────────────────────────────────────────────────────────────

/// MediaPipe FaceMesh 的眼眶点序（6 点 EAR 用的那六个）。
const LEFT_EYE: [usize; 6] = [33, 160, 158, 133, 153, 144];
const RIGHT_EYE: [usize; 6] = [362, 385, 387, 263, 373, 380];

/// 把人脸框裁出来喂给关键点模型。
///
/// 框往外扩 1.6 倍：FaceMesh 是在「整张脸带一点脖子」的取景上训练的，
/// 卡着框裁会把下巴和额头切掉，眼眶位置会飘。
fn eye_ratio(e: &Engine, img: &RgbImage, f: &FaceBox) -> Result<Option<f64>> {
    let (w, h) = img.dimensions();
    let side = (f.w.max(f.h) * 1.6).max(24.0);
    let cx = f.x + f.w / 2.0;
    let cy = f.y + f.h / 2.0;
    let x0 = (cx - side / 2.0).max(0.0) as u32;
    let y0 = (cy - side / 2.0).max(0.0) as u32;
    let x1 = (cx + side / 2.0).min(w as f32) as u32;
    let y1 = (cy + side / 2.0).min(h as f32) as u32;
    if x1 <= x0 || y1 <= y0 {
        return Ok(None);
    }

    let crop = image::imageops::crop_imm(img, x0, y0, x1 - x0, y1 - y0).to_image();
    let crop = image::imageops::resize(
        &crop,
        LMK_SIZE as u32,
        LMK_SIZE as u32,
        FilterType::Triangle,
    );

    // RGB、0–1：这是 MediaPipe 关键点模型的输入口径
    let mut input = Tensor::<f32>::zeros(&[1, LMK_SIZE, LMK_SIZE, 3]);
    for y in 0..LMK_SIZE {
        for x in 0..LMK_SIZE {
            let p = crop.get_pixel(x as u32, y as u32);
            input[[0, y, x, 0]] = p[0] as f32 / 255.0;
            input[[0, y, x, 1]] = p[1] as f32 / 255.0;
            input[[0, y, x, 2]] = p[2] as f32 / 255.0;
        }
    }

    let [out, flag] = e
        .lmk
        .run_n(
            vec![(
                e.lmk.node_id("input_12").map_err(|e| anyhow!("{e}"))?,
                input.view().into(),
            )],
            [
                e.lmk.node_id("Identity").map_err(|e| anyhow!("{e}"))?,
                e.lmk.node_id("Identity_1").map_err(|e| anyhow!("{e}"))?,
            ],
            None,
        )
        .map_err(|e| anyhow!("关键点推理失败：{e}"))?;

    // 第二个输出是「这里真有一张脸」的置信度，太低说明裁错了地方
    let flag: NdTensor<f32, 4> = flag.try_into()?;
    if flag[[0, 0, 0, 0]] < 0.5 {
        return Ok(None);
    }

    let lm: NdTensor<f32, 4> = out.try_into()?;
    let at = |i: usize| -> (f32, f32) { (lm[[0, 0, 0, i * 3]], lm[[0, 0, 0, i * 3 + 1]]) };

    let mut worst: Option<f64> = None;
    for eye in [LEFT_EYE, RIGHT_EYE] {
        let pts: Vec<(f32, f32)> = eye.iter().map(|&i| at(i)).collect();
        let r = ear(&pts);
        worst = Some(match worst {
            Some(w) => w.min(r),
            None => r,
        });
    }
    Ok(worst)
}

/// 眼睛纵横比：两组「上下眼睑距离」除以两倍的「内外眼角距离」。
///
/// 眼睛是横着的，睁得越大这个比值越大；一闭，高度塌成一条线，比值骤降。
/// 用两个距离的平均值是为了抗噪——单个距离会被一个飘掉的关键点带偏。
pub fn ear(pts: &[(f32, f32)]) -> f64 {
    if pts.len() != 6 {
        return 0.0;
    }
    let d = |a: (f32, f32), b: (f32, f32)| -> f64 {
        let dx = a.0 - b.0;
        let dy = a.1 - b.1;
        ((dx * dx + dy * dy) as f64).sqrt()
    };
    let vertical = d(pts[1], pts[5]) + d(pts[2], pts[4]);
    let horizontal = d(pts[0], pts[3]);
    if horizontal <= 0.0 {
        return 0.0;
    }
    vertical / (2.0 * horizontal)
}

/// 检查一张照片。算不出来返回 Err（调用方记下来跳过，不重试）。
pub fn eyes_for(path: &Path) -> Result<EyeReport> {
    let e = engine().map_err(|e| anyhow!("{e}"))?;
    let img = load_rgb(path).with_context(|| format!("取不到画面：{}", path.display()))?;
    let faces = detect_faces(&e, &img)?;

    let mut ratio: Option<f64> = None;
    for f in faces.iter() {
        if let Some(r) = eye_ratio(&e, &img, f)? {
            ratio = Some(match ratio {
                Some(m) => m.min(r),
                None => r,
            });
        }
    }
    Ok(EyeReport {
        faces: faces.len(),
        ratio,
    })
}

// ── 批量 ────────────────────────────────────────────────────────────────

/// 一趟批量检测的进度。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlinkProgress {
    pub done: usize,
    pub total: usize,
}

/// 一趟批量检测的结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlinkSummary {
    pub checked: usize,
    pub closed: usize,
    pub failed: usize,
    pub remaining: usize,
    pub elapsed_ms: u64,
}

/// 把库里「还没检测过」的照片过一遍。
///
/// 和画面分析同一套节奏：分批、可中断、算过的不再算。
/// `roots` 把范围限定在当前文件夹——换文件夹时旧照片留在库里（标记要复用），
/// 但不该顺带去算那些现在看不到的。
pub fn analyze_pending<F>(
    roots: &[PathBuf],
    db: &Arc<std::sync::Mutex<rusqlite::Connection>>,
    on_progress: F,
) -> Result<BlinkSummary>
where
    F: Fn(BlinkProgress) + Send + Sync,
{
    use rayon::prelude::*;

    let t0 = std::time::Instant::now();
    let targets: Vec<(i64, String)> = {
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.path FROM photos p
             WHERE p.is_primary = 1 AND p.faces IS NULL
             ORDER BY p.id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter(|(_, path)| {
                if roots.is_empty() {
                    return true;
                }
                let p = Path::new(path);
                roots.iter().any(|r| p.starts_with(r))
            })
            .collect()
    };

    let total = targets.len();
    if total == 0 {
        return Ok(BlinkSummary {
            elapsed_ms: t0.elapsed().as_millis() as u64,
            ..Default::default()
        });
    }

    let done = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let on_progress = &on_progress;

    const BATCH: usize = 32;
    let mut closed_total = 0usize;
    for chunk in targets.chunks(BATCH) {
        let results: Vec<(i64, Option<EyeReport>)> = chunk
            .par_iter()
            .map(|(id, path)| {
                let r = eyes_for(Path::new(path)).ok();
                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if r.is_none() {
                    failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                (*id, r)
            })
            .collect();

        {
            let mut conn = db.lock().map_err(|e| anyhow!("{e}"))?;
            let tx = conn.transaction()?;
            {
                let mut up =
                    tx.prepare("UPDATE photos SET faces = ?1, eye_ratio = ?2 WHERE id = ?3")?;
                for (id, r) in &results {
                    let r = match r {
                        Some(r) => *r,
                        None => continue,
                    };
                    up.execute(rusqlite::params![r.faces as i64, r.ratio, id])?;
                }
            }
            tx.commit()?;
        }

        closed_total += results
            .iter()
            .filter(|(_, r)| r.is_some_and(|r| r.closed()))
            .count();
        on_progress(BlinkProgress {
            done: done.load(std::sync::atomic::Ordering::Relaxed),
            total,
        });
    }

    let remaining = {
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE is_primary = 1 AND faces IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize
    };

    Ok(BlinkSummary {
        checked: total - failed.load(std::sync::atomic::Ordering::Relaxed),
        closed: closed_total,
        failed: failed.load(std::sync::atomic::Ordering::Relaxed),
        remaining,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一只睁着的眼：高度是宽度的三分之一左右。
    fn open_eye() -> Vec<(f32, f32)> {
        vec![
            (0.0, 0.0),   // 外眼角
            (0.5, -0.35), // 上眼睑
            (1.0, -0.3),  //
            (1.5, 0.0),   // 内眼角
            (1.0, 0.3),   //
            (0.5, 0.35),  // 下眼睑
        ]
    }

    /// 闭眼：上下眼睑贴到一起，高度接近 0。
    fn closed_eye() -> Vec<(f32, f32)> {
        let mut pts = open_eye();
        for p in pts.iter_mut() {
            p.1 *= 0.12;
        }
        pts
    }

    #[test]
    fn closed_eye_scores_below_open_eye() {
        let open = ear(&open_eye());
        let shut = ear(&closed_eye());
        assert!(open > CLOSED_THRESHOLD, "睁眼的 EAR 该高于门槛：{open}");
        assert!(shut < CLOSED_THRESHOLD, "闭眼的 EAR 该低于门槛：{shut}");
        assert!(open > shut * 3.0, "两者该差出数量级：{open} vs {shut}");
    }

    #[test]
    fn ear_ignores_scale() {
        // 同一个人，脸在画面里大一倍——EAR 应该不变
        let pts = open_eye();
        let big: Vec<(f32, f32)> = pts.iter().map(|p| (p.0 * 3.0, p.1 * 3.0)).collect();
        assert!((ear(&pts) - ear(&big)).abs() < 1e-6);
    }

    #[test]
    fn nms_collapses_duplicate_boxes() {
        let b = |x: f32, score: f32| FaceBox {
            x,
            y: 0.0,
            w: 10.0,
            h: 10.0,
            score,
        };
        let kept = nms(vec![b(0.0, 0.9), b(0.5, 0.8), b(1.0, 0.95), b(100.0, 0.7)]);
        let xs: Vec<f32> = kept.iter().map(|k| k.x).collect();
        assert!(
            xs.contains(&1.0) && xs.contains(&100.0),
            "该留下分数最高的那个和不重叠的那个：{xs:?}"
        );
        assert!(!xs.contains(&0.0), "被压掉的重叠框不该留下：{xs:?}");
    }

    #[test]
    fn no_faces_means_no_blink() {
        let r = EyeReport {
            faces: 0,
            ratio: None,
        };
        assert!(!r.closed(), "没有脸就不该报闭眼");
    }

    /// 端到端抽查：需要一张有人脸的图片。
    ///
    /// ```sh
    /// SP_BLINK_SAMPLE=/path/to/portrait.jpg cargo test blink_pipeline -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要一张有人脸的素材，通过 SP_BLINK_SAMPLE 指定"]
    fn blink_pipeline_on_real_photo() {
        let Ok(path) = std::env::var("SP_BLINK_SAMPLE") else {
            eprintln!("未设置 SP_BLINK_SAMPLE，跳过");
            return;
        };
        let report = eyes_for(Path::new(&path)).expect("检测应当跑通");
        eprintln!("{path} -> {report:?}");
        assert!(report.faces > 0, "这张照片里应该能找到脸");
        assert!(report.ratio.is_some(), "找到脸就该算出 EAR");
    }
}
