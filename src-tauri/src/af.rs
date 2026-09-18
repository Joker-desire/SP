//! 对焦区域：从尼康 MakerNote 的 AFInfo2 里把对焦框读出来。
//!
//! 为什么值得单独做：选片时最想知道的就是「这张对上了吗、对在哪」。通用 EXIF
//! 里根本没有对焦点——各家都把它塞进自己的 MakerNote，尼康放在 AFInfo2。
//! 好在它给的是**传感器原始像素坐标**，画面尺寸就在同一块里，除一下就是比例。
//!
//! 布局是拿真机 RAW（Z50II，AFInfo2 版本 0402）一字节一字节对出来的，不是抄文档：
//!
//! ```text
//!   0..4     版本 "0402"（Z8/Z9 是 0400，Z6III/Zf 是 0401，同一代处理器布局一致）
//!   5        AFAreaMode（对焦区域模式，枚举见 AF_AREA_MODES）
//!   7        AFCoordinatesAvailable：1 表示下面那组坐标有效，0 表示只有点位掩码
//!   62..64   AFImageWidth     64..66  AFImageHeight
//!   66..68   AFAreaXPosition  68..70  AFAreaYPosition（是框**中心**）
//!   70..72   AFAreaWidth      72..74  AFAreaHeight
//! ```
//!
//! 因为是反推的，所以每条数据都做了合理性检查：算出来不在画面里就当没读到。
//! 宁可不画，也不能把框画错地方——画错的框比没有框更误导人。

use exif::{In, Tag, Value};
use std::io::BufReader;
use std::path::Path;

/// MakerNote 里 AFInfo2 的 tag 号。
const TAG_AFINFO2: u16 = 0x00b7;

/// 归一化后的对焦框：0–1，相对整幅画面，已按 EXIF 方向摆正，可直接贴到显示图上。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AfArea {
    /// 框中心 x
    pub x: f64,
    /// 框中心 y
    pub y: f64,
    /// 框宽（占画面宽的比例）
    pub w: f64,
    /// 框高（占画面高的比例）
    pub h: f64,
    /// 对焦区域模式的中文名，认不出来就是 None
    pub mode: Option<String>,
}

/// Z 系的对焦区域模式（AFInfo2 第 5 字节）。值来自 exiftool 的 Nikon.pm。
const AF_AREA_MODES: [(u8, &str); 9] = [
    (192, "微点 AF"),
    (193, "单点 AF"),
    (195, "广域 AF（小）"),
    (196, "广域 AF（大）"),
    (197, "自动区域 AF"),
    (204, "动态区域 AF（小）"),
    (205, "动态区域 AF（中）"),
    (206, "动态区域 AF（大）"),
    (207, "3D 跟踪"),
];

/// 按字节序读一个 u16。越界返回 0——调用方靠合理性检查兜底。
fn u16at(b: &[u8], o: usize, le: bool) -> u16 {
    if o + 2 > b.len() {
        return 0;
    }
    if le {
        u16::from_le_bytes([b[o], b[o + 1]])
    } else {
        u16::from_be_bytes([b[o], b[o + 1]])
    }
}

fn u32at(b: &[u8], o: usize, le: bool) -> u32 {
    if o + 4 > b.len() {
        return 0;
    }
    if le {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    } else {
        u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }
}

/// AFInfo2 里那组坐标的起始偏移与顺序（见模块头注释）。
const OFF_IMAGE_W: usize = 62;

/// 解析 AFInfo2 这一块数据，得到**未考虑 EXIF 方向**的归一化框。
///
/// 纯函数、不碰文件系统，所以能直接测。
pub fn parse_af_info2(b: &[u8]) -> Option<(f64, f64, f64, f64, Option<String>)> {
    if b.len() < OFF_IMAGE_W + 12 {
        return None;
    }
    // 只认这一代处理器的布局；别的机型宁可返回 None
    if !matches!(&b[0..4], b"0400" | b"0401" | b"0402") {
        return None;
    }
    if b[7] != 1 {
        return None; // 相机没写坐标（多半是只记录了点位掩码）
    }

    let mode = AF_AREA_MODES
        .iter()
        .find(|(k, _)| *k == b[5])
        .map(|(_, s)| (*s).to_string());

    let iw = u16at(b, OFF_IMAGE_W, true) as f64;
    let ih = u16at(b, OFF_IMAGE_W + 2, true) as f64;
    let cx = u16at(b, OFF_IMAGE_W + 4, true) as f64;
    let cy = u16at(b, OFF_IMAGE_W + 6, true) as f64;
    let w = u16at(b, OFF_IMAGE_W + 8, true) as f64;
    let h = u16at(b, OFF_IMAGE_W + 10, true) as f64;

    // 合理性检查：画面尺寸得像回事，框要在画面里、还得有面积
    if iw < 100.0 || ih < 100.0 || w <= 0.0 || h <= 0.0 {
        return None;
    }
    if cx < 0.0 || cx > iw || cy < 0.0 || cy > ih || w > iw || h > ih {
        return None;
    }

    Some((cx / iw, cy / ih, w / iw, h / ih, mode))
}

/// 把「传感器方向」的框按 EXIF orientation 摆正，得到显示方向上的框。
///
/// 只处理 1/3/6/8 这四种（相机实际只会写这几种）：
/// - 1 不转；3 转 180°；
/// - 6 顺时针 90°：原来的右边变成下边；
/// - 8 逆时针 90°：原来的左边变成下边。
pub fn apply_orientation(x: f64, y: f64, w: f64, h: f64, orientation: i64) -> (f64, f64, f64, f64) {
    match orientation {
        3 => (1.0 - x, 1.0 - y, w, h),
        6 => (1.0 - y, x, h, w),
        8 => (y, 1.0 - x, h, w),
        _ => (x, y, w, h),
    }
}

/// 尼康 MakerNote 是「Nikon\0 + 版本号 + 一个标准 TIFF」，
/// TIFF 从整个 MakerNote 的第 10 个字节开始，里面的偏移都相对这个起点。
fn makernote_tiff(mn: &[u8]) -> Option<&[u8]> {
    if mn.len() < 18 || &mn[0..6] != b"Nikon\0" {
        return None;
    }
    Some(&mn[10..])
}

/// 在 MakerNote 的 TIFF 里取某个 tag 的原始字节。
fn mn_field<'a>(tiff: &'a [u8], want: u16, le: bool) -> Option<&'a [u8]> {
    let ifd = u32at(tiff, 4, le) as usize;
    let n = u16at(tiff, ifd, le) as usize;
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        if e + 12 > tiff.len() {
            break;
        }
        if u16at(tiff, e, le) != want {
            continue;
        }
        let typ = u16at(tiff, e + 2, le);
        let cnt = u32at(tiff, e + 4, le) as usize;
        let sz = match typ {
            1 | 2 | 6 | 7 => cnt,
            3 | 8 => cnt * 2,
            4 | 9 | 11 => cnt * 4,
            5 | 10 => cnt * 8,
            _ => cnt,
        };
        return if sz <= 4 {
            Some(&tiff[e + 8..e + 8 + sz.min(4)])
        } else {
            let off = u32at(tiff, e + 8, le) as usize;
            if off + sz <= tiff.len() {
                Some(&tiff[off..off + sz])
            } else {
                None
            }
        };
    }
    None
}

/// 读一张照片的对焦框。读不到（不是尼康 / 机型没写 / 模式不支持）就返回 None。
pub fn af_area_of(path: &Path, orientation: i64) -> Option<AfArea> {
    let f = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut BufReader::new(f))
        .ok()?;
    let mn = exif.get_field(Tag::MakerNote, In::PRIMARY)?;
    let bytes: &[u8] = match &mn.value {
        Value::Undefined(d, _) => d,
        _ => return None,
    };
    let tiff = makernote_tiff(bytes)?;
    // MakerNote 自带的 TIFF 头里有字节序标记；解析函数只认小端，
    // 真碰上大端的机身就把每两个字节翻过来
    let le = tiff.first() == Some(&b'I');
    let raw = mn_field(tiff, TAG_AFINFO2, le)?;
    let swapped;
    let raw: &[u8] = if le {
        raw
    } else {
        let mut v = raw.to_vec();
        for c in v.chunks_mut(2) {
            c.reverse();
        }
        swapped = v;
        &swapped
    };
    let (x, y, w, h, mode) = parse_af_info2(raw)?;
    let (x, y, w, h) = apply_orientation(x, y, w, h, orientation);
    Some(AfArea { x, y, w, h, mode })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 Z50II 实测的布局拼一块 AFInfo2：5568×3712 的画面，
    /// 对焦框中心 (3447, 3249)、尺寸 144×146，模式自动区域（197）。
    fn sample(mode: u8, coords: u8) -> Vec<u8> {
        let mut b = vec![0u8; 80];
        b[0..4].copy_from_slice(b"0402");
        b[5] = mode;
        b[7] = coords;
        b[62..64].copy_from_slice(&5568u16.to_le_bytes());
        b[64..66].copy_from_slice(&3712u16.to_le_bytes());
        b[66..68].copy_from_slice(&3447u16.to_le_bytes());
        b[68..70].copy_from_slice(&3249u16.to_le_bytes());
        b[70..72].copy_from_slice(&144u16.to_le_bytes());
        b[72..74].copy_from_slice(&146u16.to_le_bytes());
        b
    }

    #[test]
    fn reads_the_real_layout() {
        let (x, y, w, h, mode) = parse_af_info2(&sample(197, 1)).expect("应当解析出对焦框");
        assert!((x - 3447.0 / 5568.0).abs() < 1e-6, "x={x}");
        assert!((y - 3249.0 / 3712.0).abs() < 1e-6, "y={y}");
        assert!((w - 144.0 / 5568.0).abs() < 1e-6, "w={w}");
        assert!((h - 146.0 / 3712.0).abs() < 1e-6, "h={h}");
        assert_eq!(mode.as_deref(), Some("自动区域 AF"));
    }

    #[test]
    fn refuses_what_it_cannot_trust() {
        // 相机没写坐标
        assert!(parse_af_info2(&sample(197, 0)).is_none());
        // 认不出的版本
        let mut b = sample(197, 1);
        b[0..4].copy_from_slice(b"0200");
        assert!(parse_af_info2(&b).is_none());
        // 框跑到画面外
        let mut b = sample(197, 1);
        b[66..68].copy_from_slice(&60000u16.to_le_bytes());
        assert!(parse_af_info2(&b).is_none());
        // 数据太短
        assert!(parse_af_info2(&sample(197, 1)[..40]).is_none());
    }

    #[test]
    fn orientation_spins_the_box() {
        let (x, y, w, h) = (0.25, 0.75, 0.1, 0.2);
        // 不转
        let r = apply_orientation(x, y, w, h, 1);
        assert!((r.0 - x).abs() < 1e-9 && (r.1 - y).abs() < 1e-9);
        // 180°：中心对称，宽高不变
        let r = apply_orientation(x, y, w, h, 3);
        assert!((r.0 - 0.75).abs() < 1e-9 && (r.1 - 0.25).abs() < 1e-9);
        assert!((r.2 - w).abs() < 1e-9 && (r.3 - h).abs() < 1e-9);
        // 90°：宽高要跟着换
        let r6 = apply_orientation(x, y, w, h, 6);
        let r8 = apply_orientation(x, y, w, h, 8);
        assert!((r6.2 - h).abs() < 1e-9 && (r6.3 - w).abs() < 1e-9);
        assert!((r8.2 - h).abs() < 1e-9 && (r8.3 - w).abs() < 1e-9);
        // 6 和 8 是互逆的：先转 6 再转 8 回到原地
        let back = apply_orientation(r6.0, r6.1, r6.2, r6.3, 8);
        assert!((back.0 - x).abs() < 1e-9 && (back.1 - y).abs() < 1e-9);
    }

    /// 在真实素材上抽查：设 SP_SAMPLE_DIR 指向一个装有尼康 RAW 的目录时才跑，
    /// 免得 CI 上没有素材就红。
    #[test]
    fn sample_raw_on_disk() {
        let Ok(dir) = std::env::var("SP_SAMPLE_DIR") else {
            return;
        };
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            let is_raw = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("nef"));
            if !is_raw {
                continue;
            }
            if let Some(af) = af_area_of(&p, 1) {
                assert!(
                    (0.0..=1.0).contains(&af.x) && (0.0..=1.0).contains(&af.y),
                    "{} 的对焦框跑出画面：{af:?}",
                    p.display()
                );
                println!(
                    "{}: 中心 ({:.3}, {:.3}) 尺寸 {:.3}×{:.3} {:?}",
                    p.file_name().unwrap().to_string_lossy(),
                    af.x,
                    af.y,
                    af.w,
                    af.h,
                    af.mode
                );
                checked += 1;
            }
            if checked >= 3 {
                break;
            }
        }
    }
}
