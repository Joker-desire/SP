//! 缩略图生成与磁盘缓存。
//!
//! 核心认知（方案第七节）：**选片只需要「内嵌 JPEG 预览 + 元数据」，不需要 demosaic。**
//!
//! 相机在写 RAW 的时候，已经顺手存了一张全尺寸 JPEG 预览在文件里。把它抠出来，
//! 比任何 RAW 解码库都快（10-30 倍），而且色彩就是那台机身的原味。
//! 这也意味着「NEF 能不能显示」只取决于一件事：能不能正确抠出内嵌预览——
//! 而这跟相机型号无关，任何 RAW 格式都吃得下。

use anyhow::{anyhow, Context, Result};
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 三级缓存尺寸。micro 给时间线/胶片条，grid 给网格，loupe 给单张查看。
pub const SIZE_MICRO: u32 = 160;
pub const SIZE_GRID: u32 = 512;
pub const SIZE_LOUPE: u32 = 1600;

/// 放大到 100% 时按需生成的一档。
///
/// 内嵌预览往往是全尺寸的（两千多万像素），1600px 放大两倍就糊了，
/// 而「这张对上焦没有」恰恰要在 100% 下才看得准。所以留一档大的，
/// 但只在真的放大时才生成——常规浏览不该为它付磁盘和解码的钱。
pub const SIZE_PREVIEW: u32 = 4096;

/// 内嵌预览的最小宽度。低于这个值的候选是 EXIF 里的小缩略图（通常是 160px），不采用。
const MIN_PREVIEW_WIDTH: u32 = 640;

/// 单个候选 JPEG 的解析上限，防止把大块非 JPEG 数据误判成一张图。
const MAX_CANDIDATE_BYTES: usize = 64 * 1024 * 1024;

/// 一段候选 JPEG 在文件里的位置和尺寸。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JpegSpan {
    /// 起始偏移（指向 SOI 的 0xFF）
    pub start: usize,
    /// 结束偏移（EOI 之后一位，即切片上界）
    pub end: usize,
    pub width: u32,
    pub height: u32,
}

impl JpegSpan {
    pub fn len(&self) -> usize {
        self.end - self.start
    }
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// 从文件里读出最好的一张 JPEG。
///
/// 返回 `(字节, 走的路由)`。路由用于诊断——出问题时能一眼看出是抠预览失败
/// 还是别的原因，写进 `photos.decode_path`。
pub fn best_jpeg_from_file(path: &Path) -> Result<(Vec<u8>, &'static str)> {
    let file = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    // mmap：不把几十 MB 的 RAW 拷进内存，让系统按需分页。
    let map =
        unsafe { Mmap::map(&file) }.with_context(|| format!("无法映射 {}", path.display()))?;

    match best_jpeg(&map) {
        Some(span) => {
            let route = if span.start == 0 {
                "file-jpeg"
            } else {
                "embedded-jpeg"
            };
            Ok((map[span.start..span.end].to_vec(), route))
        }
        None => Err(anyhow!(
            "在文件里找不到可用的 JPEG 预览（{}）",
            path.display()
        )),
    }
}

/// 扫描整段数据，返回面积最大的那张合法 JPEG。
///
/// 「面积最大」是关键：NEF 里往往存着不止一张 JPEG——EXIF 里有个 160px 小缩略图，
/// 后面还有全尺寸预览。取最大才不会挑错。
pub fn best_jpeg(data: &[u8]) -> Option<JpegSpan> {
    let mut best: Option<JpegSpan> = None;

    let mut i = 0usize;
    while i + 3 < data.len() {
        // SOI 后面必然紧跟一个标记，所以是 FF D8 FF
        if data[i] == 0xFF && data[i + 1] == 0xD8 && data[i + 2] == 0xFF {
            if let Some(rel) = parse_jpeg_span(&data[i..]) {
                // parse_jpeg_span 返回的是相对切片的位置，这里换算成文件内绝对偏移
                let span = JpegSpan {
                    start: i,
                    end: i + rel.end,
                    width: rel.width,
                    height: rel.height,
                };
                if span.len() <= MAX_CANDIDATE_BYTES && span.width >= MIN_PREVIEW_WIDTH {
                    if best.map_or(true, |b| span.pixels() > b.pixels()) {
                        best = Some(span);
                    }
                }
                // 从这张图的结尾继续找，避免在大图内部反复匹配
                i = span.end.max(i + 2);
                continue;
            }
        }
        i += 1;
    }

    best
}

/// 解析一张 JPEG，返回它在 `data` 里的结束位置和宽高。
///
/// **必须按标记段解析，不能简单地找「SOI 之后的第一个 EOI」。**
/// RAW 里的预览本身带 EXIF，EXIF (APP1) 里可能又嵌着一张缩略图 JPEG；
/// 如果碰巧撞上那张缩略图的 EOI，就会截出一段残缺数据。
/// 正确做法是把 APP1 当成不透明载荷整段跳过。
fn parse_jpeg_span(data: &[u8]) -> Option<JpegSpan> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }

    let mut i = 2usize;
    let mut width = 0u32;
    let mut height = 0u32;

    loop {
        if i + 1 >= data.len() {
            return None;
        }
        if data[i] != 0xFF {
            return None; // 标记必须以 0xFF 开头，否则说明这段不是规整的 JPEG
        }
        // 允许 0xFF 填充字节
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }
        let marker = data[i];
        i += 1;

        match marker {
            // 无长度字段的标记：TEM、RSTn
            0x01 | 0xD0..=0xD7 => continue,
            // 又遇到 SOI：结构异常，放弃（嵌套图由 APP1 整段跳过处理，不该走到这里）
            0xD8 => return None,
            // EOI：图还没扫完就结束了
            0xD9 => return None,
            0xDA => {
                // SOS：先跳过扫描头，再在熵编码数据里找 EOI
                let len = read_len(data, i)?;
                let next = i.checked_add(len)?;
                if next > data.len() {
                    return None;
                }
                i = next;

                while i + 1 < data.len() {
                    if data[i] == 0xFF {
                        let m = data[i + 1];
                        match m {
                            0xD9 => {
                                if width > 0 && height > 0 {
                                    return Some(JpegSpan {
                                        start: 0,
                                        end: i + 2,
                                        width,
                                        height,
                                    });
                                }
                                return None;
                            }
                            // FF00 是转义、RSTn 是重同步标记，都继续扫
                            0x00 | 0xD0..=0xD7 => {
                                i += 2;
                                continue;
                            }
                            _ => break, // 熵数据里出现别的标记：退回外层按段解析
                        }
                    }
                    i += 1;
                }
                if i + 1 >= data.len() {
                    return None;
                }
                // 外层循环从当前位置继续（i 指向 0xFF），确保有推进、不死循环
                continue;
            }
            _ => {
                let len = read_len(data, i)?;
                // SOF0..SOF15 带尺寸，其中 C4=DHT / C8=JPG / CC=DAC 不是 SOF
                if (0xC0..=0xCF).contains(&marker)
                    && marker != 0xC4
                    && marker != 0xC8
                    && marker != 0xCC
                {
                    // 段结构：len(2) 精度(1) 高(2) 宽(2)
                    if len < 7 {
                        return None;
                    }
                    let h = u16::from_be_bytes([data[i + 3], data[i + 4]]) as u32;
                    let w = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
                    if w > 0 && h > 0 {
                        width = w;
                        height = h;
                    }
                }
                let next = i.checked_add(len)?;
                if next > data.len() {
                    return None;
                }
                i = next;
            }
        }
    }
}

fn read_len(data: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= data.len() {
        return None;
    }
    let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
    // 长度字段自身占 2 字节，小于 2 不合法
    if len < 2 {
        return None;
    }
    Some(len)
}

// ---------------------------------------------------------------------------
// 生成与缓存
// ---------------------------------------------------------------------------

/// 一次缩略图请求的结果，兼作诊断信息。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThumbInfo {
    pub path: PathBuf,
    /// embedded-jpeg / file-jpeg / data-url
    pub route: String,
    pub source_width: u32,
    pub source_height: u32,
    pub from_cache: bool,
}

/// 缓存键：指纹 + 尺寸。指纹变了（文件被改过）键就变，自然失效。
fn cache_key(fingerprint: &str, size: u32) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(fingerprint.as_bytes());
    hasher.update(b"|");
    hasher.update(size.to_string().as_bytes());
    hasher.finalize().to_hex()[..20].to_string()
}

/// 缓存文件路径：按哈希前两位分桶，避免单目录塞满几万个文件。
fn cache_path(root: &Path, fingerprint: &str, size: u32) -> PathBuf {
    let key = cache_key(fingerprint, size);
    root.join(&key[..2]).join(format!("{key}-{size}.jpg"))
}

/// 解码一张照片，按降级链依次尝试，最后兜底到占位图。
///
/// 四级：
///
/// 1. `embedded-jpeg` / `file-jpeg` —— 抠内嵌预览。最快，绝大多数照片走这里，
///    而且色彩就是机身原味（相机自己写的 JPEG）。
/// 2. `image-decode` —— 文件本身就是标准位图（PNG / TIFF / 没有内嵌预览的 JPEG）。
/// 3. `raw-decode` —— 完整 RAW 解码（demosaic）。慢一个数量级，但覆盖面广，
///    专门对付「RAW 里没存预览」或预览被裁掉的情况。
/// 4. `placeholder` —— 以上全失败时给一张占位图，网格里至少不是一片空白。
///
/// 路由会写进 `photos.decode_path`：出问题时能一眼看出卡在哪一级，
/// 而不是只看到「这张显示不出来」。
pub fn decode_source(path: &Path) -> Result<(image::DynamicImage, &'static str)> {
    // 1. 内嵌预览
    if let Ok((bytes, route)) = best_jpeg_from_file(path) {
        if let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg) {
            return Ok((img, route));
        }
    }

    // 2. 标准位图。NEF 之类的 RAW 在这里必然失败（image crate 不认），不影响。
    if let Ok(img) = image::open(path) {
        return Ok((img, "image-decode"));
    }

    // 3. RAW 完整解码
    if let Ok(img) = decode_raw(path) {
        return Ok((img, "raw-decode"));
    }

    // 4. 占位图
    Ok((placeholder(1024, 683), "placeholder"))
}

/// 完整 RAW 解码：demosaic + 白平衡 + 转 sRGB。
///
/// 只做「够缩略图看」的质量：2×2 拜耳块合并成一个 RGB 像素，分辨率减半。
/// 从六千万像素减到三千万，仍远超 512px 网格的需求，而速度是全尺寸 demosaic
/// 的几十倍——这一级本来就是兜底，没必要为它付全尺寸处理的代价。
fn decode_raw(path: &Path) -> Result<image::DynamicImage> {
    let mut raw = rawler::decode_file(path).map_err(|e| anyhow!("RAW 解码失败：{e}"))?;

    // 去黑电平、按白电平归一化，数据变成 0.0..1.0 的 f32
    raw.apply_scaling()
        .map_err(|e| anyhow!("RAW 电平归一化失败：{e}"))?;

    let w = raw.width;
    let h = raw.height;
    let data = match &raw.data {
        rawler::RawImageData::Float(d) => d,
        _ => return Err(anyhow!("RAW 数据不是浮点格式")),
    };
    if w < 4 || h < 4 {
        return Err(anyhow!("RAW 尺寸异常：{w}×{h}"));
    }

    // 白平衡：机身给的是 RGBE 顺序的系数，归一化后按通道缩放
    let wb = raw.neutralwb();
    let gain = {
        let g = [wb[0], wb[1], wb[2]];
        let avg = (g[0] + g[1] + g[2]) / 3.0;
        if avg > 0.0 {
            [g[0] / avg, g[1] / avg, g[2] / avg]
        } else {
            [1.0, 1.0, 1.0]
        }
    };

    let cfa = raw.cropped_cfa();
    let out_w = w / 2;
    let out_h = h / 2;
    let mut out = image::RgbImage::new(out_w as u32, out_h as u32);

    for y in 0..out_h {
        for x in 0..out_w {
            // 2×2 块里每个位置贡献自己的颜色通道；同一通道出现多次（G 有两个）取平均
            let mut sum = [0f32; 3];
            let mut cnt = [0u32; 3];
            for dy in 0..2 {
                for dx in 0..2 {
                    let sy = y * 2 + dy;
                    let sx = x * 2 + dx;
                    if sy >= h || sx >= w {
                        continue;
                    }
                    let ch = cfa.color_at(sy, sx);
                    if ch < 3 {
                        sum[ch] += data[sy * w + sx];
                        cnt[ch] += 1;
                    }
                }
            }
            let mut rgb = [0f32; 3];
            for c in 0..3 {
                let v = if cnt[c] > 0 {
                    sum[c] / cnt[c] as f32 * gain[c]
                } else {
                    // 该通道在这块里缺失（比如非 RGGB 排列），用其它通道的均值顶上
                    let others: Vec<f32> = (0..3)
                        .filter(|&k| cnt[k] > 0)
                        .map(|k| sum[k] / cnt[k] as f32 * gain[k])
                        .collect();
                    if others.is_empty() {
                        0.0
                    } else {
                        others.iter().sum::<f32>() / others.len() as f32
                    }
                };
                rgb[c] = linear_to_srgb(v.clamp(0.0, 1.0));
            }
            out.put_pixel(
                x as u32,
                y as u32,
                image::Rgb([
                    (rgb[0] * 255.0) as u8,
                    (rgb[1] * 255.0) as u8,
                    (rgb[2] * 255.0) as u8,
                ]),
            );
        }
    }

    Ok(image::DynamicImage::ImageRgb8(out))
}

/// 线性光 → sRGB 编码。不做这一步 RAW 出来的图会黑得几乎看不见。
fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// 占位图：斜纹底 + 一个「没有预览」的方框。
///
/// 纯代码画，不引额外依赖，也不读任何字体——它只在最坏情况下出现，
/// 能让人一眼看出「这张没能解出来」，而不是看到一个空洞。
pub fn placeholder(width: u32, height: u32) -> image::DynamicImage {
    let mut img = image::RgbImage::from_pixel(width, height, image::Rgb([38, 40, 45]));

    // 斜纹：让占位图和「加载中」的纯色块区分开
    for y in 0..height {
        for x in 0..width {
            if ((x + y) / 32) % 2 == 0 {
                img.put_pixel(x, y, image::Rgb([46, 48, 54]));
            }
        }
    }

    // 中间的方框 + 一条对角线，形状类似「图片」图标被打叉
    let side = width.min(height) / 4;
    let left = width / 2 - side / 2;
    let top = height / 2 - side / 2;
    let fg = image::Rgb([120, 124, 133]);
    let stroke = (width / 256).max(2);

    for t in 0..stroke {
        for i in 0..side {
            // 上下两条边
            for &dx in &[left + i] {
                for &dy in &[top + t, top + side - 1 - t] {
                    if dx < width && dy < height {
                        img.put_pixel(dx, dy, fg);
                    }
                }
            }
            // 左右两条边
            for &dy in &[top + i] {
                for &dx in &[left + t, left + side - 1 - t] {
                    if dx < width && dy < height {
                        img.put_pixel(dx, dy, fg);
                    }
                }
            }
        }
        // 对角线（方框的斜杠）
        for i in 0..side {
            let dx = left + i;
            let dy = top + i;
            for k in 0..stroke {
                if dx < width && dy + k < height {
                    img.put_pixel(dx, dy + k, fg);
                }
            }
        }
    }

    image::DynamicImage::ImageRgb8(img)
}
///
/// 多个尺寸会复用同一次解码——解码是整条链路里最贵的一步。
/// **返回顺序与 `sizes` 一一对应**，调用方可以直接按下标取。
pub fn ensure(
    thumbs_root: &Path,
    fingerprint: &str,
    source: &Path,
    sizes: &[u32],
    orientation: Option<i64>,
) -> Result<(Vec<ThumbInfo>, &'static str)> {
    let mut slots: Vec<Option<ThumbInfo>> = sizes
        .iter()
        .map(|&size| {
            let path = cache_path(thumbs_root, fingerprint, size);
            path.is_file().then(|| ThumbInfo {
                path,
                route: "cached".into(),
                source_width: 0,
                source_height: 0,
                from_cache: true,
            })
        })
        .collect();

    if slots.iter().all(|s| s.is_some()) {
        return Ok((slots.into_iter().flatten().collect(), "cached"));
    }

    // 降级链：抠预览 → 标准位图 → RAW 解码 → 占位图。走到最后一级也不会失败。
    let (mut img, route) = decode_source(source)?;
    let source_width = img.width();
    let source_height = img.height();

    // 方向校正必须在缩放前做，否则宽高比会被搞错
    if let Some(o) = exif_orientation(orientation) {
        img.apply_orientation(o);
    }

    for (idx, &size) in sizes.iter().enumerate() {
        if slots[idx].is_some() {
            continue;
        }
        let path = cache_path(thumbs_root, fingerprint, size);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let encoded = encode_thumb(&img, size)?;
        write_atomic(&path, &encoded)?;
        slots[idx] = Some(ThumbInfo {
            path,
            route: route.to_string(),
            source_width,
            source_height,
            from_cache: false,
        });
    }

    Ok((slots.into_iter().flatten().collect(), route))
}

/// 缩放到目标尺寸并编码为 JPEG。
///
/// 用 `thumbnail()` 而不是 `resize()`：从 6000px 缩到 512px 这种大比例下采样，
/// `thumbnail()` 的快速算法质量更好也更省时间；超过原尺寸时不会放大。
fn encode_thumb(img: &image::DynamicImage, size: u32) -> Result<Vec<u8>> {
    let scaled = if img.width() <= size && img.height() <= size {
        img.clone()
    } else {
        img.thumbnail(size, size)
    };

    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85);
    encoder.encode_image(&scaled).context("缩略图编码失败")?;
    Ok(out)
}

/// 先写临时文件再改名。避免中途被杀进程时留下半张图，
/// 而半张图会被下次运行当成「已缓存」。
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("jpg.tmp");
    {
        let mut f = File::create(&tmp).with_context(|| format!("无法写入 {}", tmp.display()))?;
        f.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// EXIF 方向值（1-8）→ image crate 的方向枚举。
fn exif_orientation(value: Option<i64>) -> Option<image::metadata::Orientation> {
    use image::metadata::Orientation as O;
    Some(match value? {
        2 => O::FlipHorizontal,
        3 => O::Rotate180,
        4 => O::FlipVertical,
        5 => O::Rotate90FlipH,
        6 => O::Rotate90,
        7 => O::Rotate270FlipH,
        8 => O::Rotate270,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// 并发闸门
// ---------------------------------------------------------------------------

/// 缩略图生成的内存闸门。
///
/// 一张四千多万像素的 NEF，内嵌预览解成 RGB8 就要一百多 MB。前端一次滚屏可能
/// 同时发来几十上百个请求，而 tokio 的 blocking 线程池会全部接住——内存直接爆。
/// 所以限流放在这里，不能指望调用方自觉。
pub struct Limiter {
    inner: std::sync::Mutex<usize>,
    cv: std::sync::Condvar,
    max: usize,
}

impl Limiter {
    pub fn new(max: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(0),
            cv: std::sync::Condvar::new(),
            max: max.max(1),
        }
    }

    /// 取得一个名额，取不到就等。返回的凭证 drop 时自动归还。
    pub fn acquire(&self) -> Permit<'_> {
        let mut active = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= self.max {
            active = self.cv.wait(active).unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        Permit(self)
    }
}

pub struct Permit<'a>(&'a Limiter);

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut active = self.0.inner.lock().unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
        self.0.cv.notify_one();
    }
}

/// 全局闸门。4 路是权衡：再高内存吃不消，再低解码线程用不满。
pub fn global_limiter() -> &'static Limiter {
    static LIMITER: std::sync::OnceLock<Limiter> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| Limiter::new(4))
}

// ---------------------------------------------------------------------------
// 容量上限与 LRU 淘汰
// ---------------------------------------------------------------------------

/// 缓存容量上限。
///
/// 缩略图会一直长——每看一张就留 2~3 份 JPEG，看一万张就是好几个 GB。
/// 以前只能靠用户手动清理，现在给一个默认上限，超了自动淘汰最久没用的。
/// 这些都是可再生数据（删了重新抠一次预览就回来），所以自动删不心疼。
pub const DEFAULT_MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 淘汰水位线：清到上限的 80%，而不是刚好卡在上限。
/// 否则刚清完再生成两张又超了，会变成「每张都触发一次全目录扫描」。
const PRUNE_WATERMARK: f64 = 0.8;

/// 自动淘汰的最小间隔（秒）。扫一遍几万个文件不便宜，没必要每次生成都扫。
const PRUNE_MIN_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneStats {
    pub removed: u64,
    pub freed_bytes: u64,
}

/// 按 LRU 把缓存压回水位线以下。
///
/// 排序键用**访问时间**而不是修改时间：缩略图写完就不会再改，mtime 等于创建时间，
/// 拿它排序等于「按加入顺序删」，最近看过的老照片会被误删。atime 拿不到时
/// （某些挂载用了 noatime）退回 mtime，至少不会崩。
pub fn enforce_limit(root: &Path, max_bytes: u64) -> Result<PruneStats> {
    #[derive(Debug)]
    struct Entry {
        path: PathBuf,
        size: u64,
        atime: std::time::SystemTime,
    }

    let mut files: Vec<Entry> = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        let Ok(md) = entry.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let atime = md
            .accessed()
            .or_else(|_| md.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        files.push(Entry {
            path: entry.into_path(),
            size: md.len(),
            atime,
        });
    }

    let total: u64 = files.iter().map(|f| f.size).sum();
    if total <= max_bytes {
        return Ok(PruneStats::default());
    }

    let target = (max_bytes as f64 * PRUNE_WATERMARK) as u64;
    files.sort_by_key(|f| f.atime);

    let mut stats = PruneStats::default();
    let mut current = total;
    for f in files {
        if current <= target {
            break;
        }
        if std::fs::remove_file(&f.path).is_ok() {
            current -= f.size;
            stats.removed += 1;
            stats.freed_bytes += f.size;
        }
    }

    Ok(stats)
}

/// 惰性触发的自动淘汰：最多一分钟扫一次，且只在超限后才真的删。
///
/// 每次生成缩略图都遍历整个缓存目录是浪费——几万个文件的 stat 调用比生成一张
/// 缩略图还贵。所以限流，并且先做一次便宜的总量判断。
pub fn enforce_if_needed(root: &Path) {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if now.saturating_sub(last) < PRUNE_MIN_INTERVAL_SECS {
        return;
    }
    // 抢到锁的线程去扫，其它线程这一轮直接跳过
    if LAST
        .compare_exchange(
            last,
            now,
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return;
    }

    let _ = enforce_limit(root, DEFAULT_MAX_CACHE_BYTES);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张真实可解码的 JPEG（纯色），用于测试。
    fn make_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([120, 160, 90]),
        ));
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
        enc.encode_image(&img).unwrap();
        out
    }

    #[test]
    fn parses_plain_jpeg_dimensions() {
        let jpeg = make_jpeg(1280, 853);
        let span = parse_jpeg_span(&jpeg).expect("应该能解析");
        assert_eq!((span.width, span.height), (1280, 853));
        assert_eq!(span.end, jpeg.len(), "结束位置应该是 EOI 之后一位");
    }

    #[test]
    fn picks_largest_preview_not_the_exif_thumbnail() {
        // 模拟 NEF：头部 + 小缩略图 + 大预览 + 尾部数据
        let small = make_jpeg(160, 120);
        let big = make_jpeg(1600, 1067);

        let mut file = vec![0u8; 4096];
        file.extend_from_slice(&small);
        file.extend_from_slice(&vec![7u8; 2048]);
        file.extend_from_slice(&big);
        file.extend_from_slice(&vec![9u8; 1024]);

        let span = best_jpeg(&file).expect("应该找到预览");
        assert_eq!((span.width, span.height), (1600, 1067));
        assert_eq!(&file[span.start..span.end], &big[..]);
    }

    #[test]
    fn ignores_nested_thumbnail_inside_app1() {
        // 关键场景：大图自己的 EXIF(APP1) 里嵌了一张小缩略图。
        // 如果按「SOI 后第一个 EOI」去截，就会截出残缺数据。
        let inner = make_jpeg(120, 80);
        let mut seg = Vec::new();
        seg.extend_from_slice(&[0xFF, 0xE1]); // APP1
        let payload_len = 2 + 6 + inner.len();
        seg.extend_from_slice(&(payload_len as u16).to_be_bytes());
        seg.extend_from_slice(b"Exif\0\0");
        seg.extend_from_slice(&inner);

        // 手工拼一张带 APP1 的 JPEG：SOI + APP1 + SOF0 + SOS + 熵数据 + EOI
        let mut outer = vec![0xFF, 0xD8];
        outer.extend_from_slice(&seg);
        // SOF0: len=17, 精度8, 高1067, 宽1600, 3 分量
        outer.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        outer.extend_from_slice(&1067u16.to_be_bytes());
        outer.extend_from_slice(&1600u16.to_be_bytes());
        outer.extend_from_slice(&[0x03, 0x01, 0x11, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        // SOS: len=12
        outer.extend_from_slice(&[
            0xFF, 0xDA, 0x00, 0x0C, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03, 0x11, 0x00,
        ]);
        outer.extend_from_slice(&[0x12, 0x34, 0xFF, 0x00, 0x56, 0x78]); // 熵数据，含 FF00 转义
        outer.extend_from_slice(&[0xFF, 0xD9]);

        let span = parse_jpeg_span(&outer).expect("应该解析成功而不是截到内嵌缩略图");
        assert_eq!((span.width, span.height), (1600, 1067));
        assert_eq!(
            span.end,
            outer.len(),
            "必须终止在外层 EOI，而不是内层缩略图的 EOI"
        );
        assert!(span.len() > inner.len(), "截出来的应该比内嵌缩略图大得多");
    }

    #[test]
    fn rejects_data_without_jpeg() {
        let junk = vec![0u8; 8192];
        assert!(best_jpeg(&junk).is_none());
    }

    /// 造一个指定访问时间、指定大小的缓存文件。
    fn make_cached_file(dir: &Path, name: &str, size: usize, atime_secs: u64) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![7u8; size]).unwrap();
        let f = File::options().write(true).open(&p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_accessed(
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(atime_secs),
        ))
        .unwrap();
        p
    }

    #[test]
    fn enforce_limit_does_nothing_when_under_the_cap() {
        let dir = std::env::temp_dir().join(format!("sp-lru-under-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            make_cached_file(&dir, &format!("f{i}.jpg"), 100, 1000 + i as u64);
        }

        let stats = enforce_limit(&dir, 1000).unwrap();
        assert_eq!(stats.removed, 0);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            4,
            "没超限就不该动任何文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_limit_drops_the_least_recently_used_first() {
        let dir = std::env::temp_dir().join(format!("sp-lru-over-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 五个文件各 100 字节，访问时间依次变新
        for i in 0..5 {
            make_cached_file(&dir, &format!("f{i}.jpg"), 100, 1000 + i as u64);
        }

        // 上限 300 → 水位线 240，得删到剩下 2 个（200 字节）
        let stats = enforce_limit(&dir, 300).unwrap();
        assert_eq!(stats.removed, 3, "应删掉 3 个最旧的");
        assert_eq!(stats.freed_bytes, 300);

        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        left.sort();
        // 留下来的必须是最近访问过的两个：f3、f4
        assert_eq!(left, vec!["f3.jpg".to_string(), "f4.jpg".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_is_a_real_image() {
        let img = placeholder(320, 200);
        assert_eq!(img.width(), 320);
        assert_eq!(img.height(), 200);
        // 至少要有两种颜色，否则说明「占位图」其实是一片纯色，看不出区别
        let rgb = img.to_rgb8();
        let mut seen = std::collections::HashSet::new();
        for p in rgb.pixels().take(4000) {
            seen.insert((p[0], p[1], p[2]));
        }
        assert!(seen.len() > 1, "占位图不该是纯色块");
    }

    #[test]
    fn limiter_caps_concurrency_and_releases() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let limiter = Limiter::new(2);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);

        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let _permit = limiter.acquire();
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });

        assert_eq!(active.load(Ordering::SeqCst), 0, "名额必须全部归还");
        assert!(peak.load(Ordering::SeqCst) <= 2, "并发不能超过上限");
    }

    #[test]
    fn preview_tier_really_is_higher_resolution() {
        let dir = std::env::temp_dir().join(format!("thumb-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 比高清档再大一点，才能验证它是被缩到 4096 而不是原样输出
        let src = dir.join("big.jpg");
        std::fs::write(&src, make_jpeg(4200, 2800)).unwrap();

        let (infos, _) = ensure(&dir, "fp-big", &src, &[SIZE_PREVIEW], None).unwrap();
        let decoded = image::open(&infos[0].path).unwrap();
        assert_eq!(decoded.width(), SIZE_PREVIEW, "应该缩到高清档的宽度");
        assert!(
            decoded.width() > SIZE_LOUPE,
            "高清档必须明显大于大图档，否则放大没意义"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn thumbnail_never_upscales() {
        let jpeg = make_jpeg(320, 240);
        let img = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg).unwrap();
        let out = encode_thumb(&img, 512).unwrap();
        let back = image::load_from_memory_with_format(&out, image::ImageFormat::Jpeg).unwrap();
        assert_eq!((back.width(), back.height()), (320, 240));
    }

    #[test]
    fn builds_and_reuses_cache() {
        let dir = std::env::temp_dir().join(format!("thumb-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("sample.jpg");
        std::fs::write(&src, make_jpeg(2000, 1333)).unwrap();

        // 修掉 90° 方向，验证方向会被应用（宽高互换）
        let (infos, route) =
            ensure(&dir, "fp-abc", &src, &[SIZE_MICRO, SIZE_GRID], Some(6)).unwrap();
        assert_eq!(route, "file-jpeg");
        assert_eq!(infos.len(), 2);
        assert!(infos.iter().all(|i| !i.from_cache));
        assert!(infos.iter().all(|i| i.path.is_file()));

        let grid = infos
            .iter()
            .find(|i| i.path.to_string_lossy().contains("512"))
            .unwrap();
        let decoded = image::open(&grid.path).unwrap();
        assert!(decoded.width() <= 512 && decoded.height() <= 512);
        assert!(decoded.height() > decoded.width(), "方向校正后应该是竖图");

        // 第二次应该全部命中缓存
        let (infos2, route2) =
            ensure(&dir, "fp-abc", &src, &[SIZE_MICRO, SIZE_GRID], Some(6)).unwrap();
        assert_eq!(route2, "cached");
        assert!(infos2.iter().all(|i| i.from_cache));

        // 指纹变化 → 缓存键变化 → 不会命中旧文件
        let (infos3, _) = ensure(&dir, "fp-xyz", &src, &[SIZE_GRID], Some(6)).unwrap();
        assert!(!infos3[0].from_cache);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 拿真实素材抽查并发吞吐量。用来回答一个很实际的问题：
    /// **「首屏 30 张要等多久」**。单张耗时乘以张数会算得过于悲观，
    /// 因为真正的瓶颈是「整张预览解码」，而这一步是能并行的。
    ///
    /// ```bash
    /// SP_SAMPLE_DIR=/path/to/shoot \
    ///   cargo test thumb_throughput -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要真实素材目录，通过 SP_SAMPLE_DIR 指定"]
    fn thumb_throughput() {
        let Ok(dir) = std::env::var("SP_SAMPLE_DIR") else {
            return;
        };

        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .expect("素材目录读不到")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    crate::pairing::classify(&crate::pairing::ext_lower(p)),
                    Some(crate::pairing::FileKind::Raw)
                )
            })
            .collect();
        files.sort();
        files.truncate(24);
        assert!(!files.is_empty(), "目录里没有 RAW 文件：{dir}");

        let root = std::env::temp_dir().join("sp-throughput");
        let _ = std::fs::remove_dir_all(&root);

        let n = files.len();
        let t0 = std::time::Instant::now();
        // 故意用和运行时同一道闸门，量出来的才是真实并发度
        std::thread::scope(|s| {
            for (i, f) in files.iter().enumerate() {
                let root = &root;
                s.spawn(move || {
                    let _permit = global_limiter().acquire();
                    let out = ensure(
                        root,
                        &format!("tp-{i:03}"),
                        f,
                        &[SIZE_GRID, SIZE_MICRO],
                        None,
                    );
                    assert!(out.is_ok(), "生成失败 {}：{:?}", f.display(), out.err());
                });
            }
        });
        let secs = t0.elapsed().as_secs_f64();

        println!(
            "{n} 张 → {secs:.1} 秒（{:.2} 张/秒，4 路并发）；折算首屏 30 张约 {:.1} 秒",
            n as f64 / secs,
            30.0 / (n as f64 / secs)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 拿真实素材抽查。合成样本证明不了「真机能用」——不同机身的 NEF 布局、
    /// 内嵌预览的尺寸和数量都不一样，必须用真文件跑一次。
    ///
    /// ```bash
    /// SP_SAMPLE=/path/to/DSC_0001.NEF \
    ///   cargo test real_nef -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要真实素材，通过 SP_SAMPLE 指定"]
    fn real_nef_smoke() {
        let Ok(sample) = std::env::var("SP_SAMPLE") else {
            eprintln!("未设置 SP_SAMPLE，跳过");
            return;
        };
        let path = PathBuf::from(sample);
        let size_mb = std::fs::metadata(&path).unwrap().len() as f64 / 1024.0 / 1024.0;

        let t0 = std::time::Instant::now();
        let (bytes, route) = best_jpeg_from_file(&path).expect("真实 NEF 应该能抠出预览");
        let t_extract = t0.elapsed();

        let t1 = std::time::Instant::now();
        let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg)
            .expect("抠出来的应该是合法 JPEG");
        let t_decode = t1.elapsed();

        println!(
            "源文件 {:.1}MB → 路由 {}，预览 {}KB，{}×{}",
            size_mb,
            route,
            bytes.len() / 1024,
            img.width(),
            img.height()
        );
        println!("耗时：抠预览 {:?} · 解码 {:?}", t_extract, t_decode);
        assert!(img.width() >= MIN_PREVIEW_WIDTH, "预览太小，可能抠错了段");

        let root = std::env::temp_dir().join("sp-real-smoke");
        let _ = std::fs::remove_dir_all(&root);
        let t2 = std::time::Instant::now();
        let (infos, _) = ensure(
            &root,
            "real-smoke",
            &path,
            &[SIZE_MICRO, SIZE_GRID, SIZE_LOUPE],
            Some(8),
        )
        .expect("三级缩略图都应该生成成功");
        println!("整条链路（抠+解码+三级缩放编码）{:?}", t2.elapsed());
        assert_eq!(infos.len(), 3);
        for info in &infos {
            let kb = std::fs::metadata(&info.path).unwrap().len() / 1024;
            println!("  → {:?}  {kb}KB", info.path.file_name().unwrap());
            assert!(kb > 0);
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
