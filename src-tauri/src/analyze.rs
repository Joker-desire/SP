//! 画面分析：清晰度与曝光。
//!
//! 为什么单独一个模块、而且不在扫描时顺手算掉：
//!
//! 扫描的KPI是「快点把照片铺到网格上」，它连文件都不解码（指纹只读前 1MB）。
//! 分析必须解码，一张几百毫秒到几十毫秒不等，几千张够把首次导入拖到几分钟——
//! 正是用户抱怨过的那种「干等着」。所以这里走**扫描之后的后台趟**：
//! 分批、可中断、算过的不再算，网格先出图，分析结果慢慢补上来。
//!
//! 数据只从 RAW 内嵌的 JPEG 预览里取（`thumb::best_jpeg_from_file`）。
//! 一是快一个数量级，二是内嵌预览带机身自己的色彩处理，判断过曝更接近肉眼。

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::thumb;

/// 分析用的工作尺寸。清晰度看的是边缘对比，256px 足够；再大只是白烧 CPU。
const WORK_SIZE: u32 = 256;

/// 清晰度的判定门槛（0–100）。低于它算「可能糊了」。
///
/// 这个数不是理论推导出来的，是给摄影师看的经验值：宁可漏标一张，
/// 也不能把好片标成糊——误标的代价（重新看一遍）比漏标高。
/// 等真素材跑出分布之后再校准。
pub const BLUR_THRESHOLD: f64 = 25.0;
/// 高光溢出 / 暗部死黑的判定门槛（占比）。
pub const OVEREXPOSED_THRESHOLD: f64 = 0.02;
pub const UNDEREXPOSED_THRESHOLD: f64 = 0.25;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Metrics {
    /// 0–100，越高越清晰
    pub sharpness: f64,
    /// 接近纯白的像素占比 0–1
    pub overexposed: f64,
    /// 接近纯黑的像素占比 0–1
    pub underexposed: f64,
}

/// 算一张照片的画面指标。算不出来返回 Err——调用方记下来跳过，不重试。
pub fn metrics_for(path: &Path) -> Result<Metrics> {
    let (bytes, _) = thumb::best_jpeg_from_file(path)
        .with_context(|| format!("取不到预览：{}", path.display()))?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
        .with_context(|| format!("预览解码失败：{}", path.display()))?;
    Ok(measure(&img))
}

/// 从已经解码的图像算指标。单独拆出来是为了能直接测——
/// 清晰度这种「感觉对不对」的东西，没有数值样本很容易写反。
pub fn measure(img: &image::DynamicImage) -> Metrics {
    // 先缩到工作尺寸：清晰度是相对量，缩完照样能分出糊和不糊，还快得多
    let small = img.resize(WORK_SIZE, WORK_SIZE, image::imageops::FilterType::Triangle);
    let gray = small.to_luma8();
    let (w, h) = gray.dimensions();
    if w < 3 || h < 3 {
        return Metrics::default();
    }

    let sharpness = laplacian_score(&gray);

    let mut over = 0u64;
    let mut under = 0u64;
    for p in gray.as_raw() {
        // 用 0–255 的灰度直接判：内嵌预览已经做过机身色彩处理，
        // 高光溢出的观感和这个阈值对得上
        if *p >= 250 {
            over += 1;
        } else if *p <= 5 {
            under += 1;
        }
    }
    let total = (w as u64) * (h as u64);

    Metrics {
        sharpness,
        overexposed: over as f64 / total as f64,
        underexposed: under as f64 / total as f64,
    }
}

/// 拉普拉斯方差，归一到 0–100。
///
/// 原理：清晰的地方相邻像素变化大，糊成一片的地方变化小。对每个像素算
/// 「自己×4 减去上下左右」，取这批数的方差——方差越大越清晰。
/// 原始方差没有上界，除以一个经验常数压到 0–100，方便显示和定门槛。
fn laplacian_score(gray: &image::GrayImage) -> f64 {
    let (w, h) = gray.dimensions();
    let px = |x: u32, y: u32| -> i32 { gray.get_pixel(x, y).0[0] as i32 };

    let mut sum = 0f64;
    let mut sum_sq = 0f64;
    let mut n = 0f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let lap = 4 * px(x, y) - px(x - 1, y) - px(x + 1, y) - px(x, y - 1) - px(x, y + 1);
            sum += lap as f64;
            sum_sq += (lap * lap) as f64;
            n += 1.0;
        }
    }
    if n == 0.0 {
        return 0.0;
    }
    let mean = sum / n;
    let variance = (sum_sq / n - mean * mean).max(0.0);
    // 开方再压到 0–100。
    //
    // 原始方差是平方量，动态范围太夸张：清晰图和极致清晰图能差出几十倍，
    // 而「稍微糊」和「很糊」只差几倍——线性映射会把可用的区间全挤在底部。
    // 开方之后更接近「看起来差多少」。
    //
    // 门槛是拍脑袋定的经验值，不是理论推导：先用一批真素材跑一遍看分布再调。
    // 宁可先标得保守一点，误标一张好片比漏标一张糊片更烦人。
    variance.sqrt().clamp(0.0, 100.0)
}

/// 一趟分析的进度。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeProgress {
    pub done: usize,
    pub total: usize,
}

/// 一趟分析的结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeSummary {
    /// 这一趟新算了多少张
    pub analyzed: usize,
    /// 算不出来（取不到预览）的张数
    pub failed: usize,
    /// 库里还有多少张没算过——通常是没跑完就被中断了
    pub remaining: usize,
    pub elapsed_ms: u64,
}

/// 把库里「还没分析过」的照片算一遍。
///
/// `roots` 用来把范围限定在当前文件夹——换文件夹时旧照片留在库里（标记要复用），
/// 但分析不该顺带去算那些现在看不到的。
pub fn analyze_pending<F>(
    roots: &[std::path::PathBuf],
    db: &Arc<Mutex<Connection>>,
    on_progress: F,
) -> Result<AnalyzeSummary>
where
    F: Fn(AnalyzeProgress) + Send + Sync,
{
    let t0 = std::time::Instant::now();

    // 只挑主文件：一次快门算一张就够，NEF 和 JPG 的清晰度是一样的
    let targets: Vec<(i64, String)> = {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stmt = conn.prepare(
            "SELECT p.id, p.path FROM photos p
             WHERE p.is_primary = 1 AND p.sharpness IS NULL
             ORDER BY p.id",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // 目录范围在 Rust 侧过滤：roots 是路径前缀，拼 SQL 又要把长度交给 SQLite，
        // 这里张数不多，直接比更清楚
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
        return Ok(AnalyzeSummary {
            analyzed: 0,
            failed: 0,
            remaining: 0,
            elapsed_ms: t0.elapsed().as_millis() as u64,
        });
    }

    let done = std::sync::atomic::AtomicUsize::new(0);
    let failed = std::sync::atomic::AtomicUsize::new(0);
    let on_progress = &on_progress;

    // 分批写库：每批结束就放锁，前端该查还是能查
    const BATCH: usize = 100;
    for chunk in targets.chunks(BATCH) {
        let results: Vec<(i64, Option<Metrics>)> = chunk
            .par_iter()
            .map(|(id, path)| {
                let m = metrics_for(Path::new(path)).ok();
                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if m.is_none() {
                    failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                (*id, m)
            })
            .collect();

        {
            let mut conn = db.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
            let tx = conn.transaction()?;
            {
                let mut up = tx.prepare(
                    "UPDATE photos SET sharpness = ?1, overexposed = ?2, underexposed = ?3
                     WHERE id = ?4",
                )?;
                for (id, m) in &results {
                    let m = match m {
                        Some(m) => *m,
                        None => continue,
                    };
                    up.execute(rusqlite::params![
                        m.sharpness,
                        m.overexposed,
                        m.underexposed,
                        id
                    ])?;
                }
            }
            tx.commit()?;
        }

        on_progress(AnalyzeProgress {
            done: done.load(std::sync::atomic::Ordering::Relaxed),
            total,
        });
    }

    let remaining = {
        let conn = db.lock().map_err(|e| anyhow::anyhow!("{e}"))?;
        conn.query_row(
            "SELECT COUNT(*) FROM photos WHERE is_primary = 1 AND sharpness IS NULL",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize
    };

    Ok(AnalyzeSummary {
        analyzed: total - failed.load(std::sync::atomic::Ordering::Relaxed),
        failed: failed.load(std::sync::atomic::Ordering::Relaxed),
        remaining,
        elapsed_ms: t0.elapsed().as_millis() as u64,
    })
}

use rayon::prelude::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn noisy(w: u32, h: u32) -> image::DynamicImage {
        // 高频噪点图＝到处都是边缘＝「清晰」
        let mut img = image::GrayImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            let v = ((x * 7 + y * 13) % 251) as u8;
            *p = image::Luma([v]);
        }
        image::DynamicImage::ImageLuma8(img)
    }

    fn flat(w: u32, h: u32) -> image::DynamicImage {
        // 纯色＝完全没有边缘＝「糊」
        image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(w, h, image::Luma([128u8])))
    }

    #[test]
    fn sharp_beats_blurry() {
        let s_sharp = measure(&noisy(256, 256)).sharpness;
        let s_flat = measure(&flat(256, 256)).sharpness;
        assert!(
            s_sharp > s_flat + 10.0,
            "清晰图该明显高过纯色图：{s_sharp} vs {s_flat}"
        );
        assert!(s_flat < BLUR_THRESHOLD, "纯色图该被判成糊");
    }

    #[test]
    fn counts_over_and_under_exposed_pixels() {
        let mut img = image::GrayImage::new(100, 100);
        for (i, p) in img.pixels_mut().enumerate() {
            // 前 1000 个纯白（10%），后 3000 个纯黑（30%），其余中灰
            let v = if i < 1000 {
                255u8
            } else if i < 4000 {
                0u8
            } else {
                128u8
            };
            *p = image::Luma([v]);
        }
        let m = measure(&image::DynamicImage::ImageLuma8(img));
        assert!(
            (m.overexposed - 0.10).abs() < 0.02,
            "过曝占比 {:.3}",
            m.overexposed
        );
        assert!(
            (m.underexposed - 0.30).abs() < 0.02,
            "欠曝占比 {:.3}",
            m.underexposed
        );
    }
}
