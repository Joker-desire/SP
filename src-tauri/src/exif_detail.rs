//! 完整 EXIF 读取。
//!
//! 索引阶段（indexer.rs）为了几千张的吞吐，只抽了十来个字段写库。
//! 详情面板是单张操作，一次读一个文件，可以放开手把 EXIF 里的东西尽量摆出来。
//!
//! 这里刻意**不写回数据库**：全量 EXIF 体积大、格式杂，存下来只会让库膨胀，
//! 而它只在「打开详情看一眼」时才被需要——现读现用正好。

use anyhow::{Context, Result};
use exif::{Field, In, Tag, Value};
use serde::Serialize;
use std::path::Path;

/// 详情面板里的一行。group 用来在前端分小节，顺序由后端决定。
#[derive(Debug, Clone, Serialize)]
pub struct ExifItem {
    pub group: String,
    pub label: String,
    pub value: String,
}

/// 读一个文件的全部 EXIF，按小节顺序返回。文件读不了 / 没有 EXIF 就返回空。
pub fn read_full(path: &Path) -> Result<Vec<ExifItem>> {
    let file = std::fs::File::open(path).with_context(|| format!("打开失败 {}", path.display()))?;
    let mut br = std::io::BufReader::new(file);
    let exif = exif::Reader::new()
        .read_from_container(&mut br)
        .with_context(|| format!("读取 EXIF 失败 {}", path.display()))?;

    let mut out: Vec<ExifItem> = Vec::new();
    let mut push = |group: &str, label: &str, v: Option<String>| {
        if let Some(value) = v {
            let value = value.trim().to_string();
            if !value.is_empty() {
                out.push(ExifItem {
                    group: group.into(),
                    label: label.into(),
                    value,
                });
            }
        }
    };

    // ── 曝光 ────────────────────────────────────────────────────────────────
    let g = "曝光";
    push(
        g,
        "曝光程序",
        enum_of(&exif, Tag::ExposureProgram, EXPOSURE_PROGRAM),
    );
    push(
        g,
        "曝光模式",
        enum_of(&exif, Tag::ExposureMode, EXPOSURE_MODE),
    );
    push(
        g,
        "曝光补偿",
        rational_of(&exif, Tag::ExposureBiasValue).map(|v| {
            if (v - 0.0).abs() < 1e-9 {
                "0 EV".into()
            } else {
                format!("{}{:.2} EV", if v > 0.0 { "+" } else { "" }, v)
            }
        }),
    );
    push(
        g,
        "测光模式",
        enum_of(&exif, Tag::MeteringMode, METERING_MODE),
    );
    push(g, "闪光灯", flash(&exif));
    push(
        g,
        "亮度（APEX）",
        rational_of(&exif, Tag::BrightnessValue).map(|v| format!("{v:.2}")),
    );
    push(
        g,
        "推荐曝光指数",
        uint_of(&exif, Tag::RecommendedExposureIndex).map(|v| v.to_string()),
    );

    // ── 镜头与焦段 ──────────────────────────────────────────────────────────
    let g = "镜头";
    push(g, "镜头品牌", ascii_of(&exif, Tag::LensMake));
    push(g, "镜头型号", ascii_of(&exif, Tag::LensModel));
    push(g, "镜头序列号", ascii_of(&exif, Tag::LensSerialNumber));
    push(
        g,
        "焦距",
        rational_of(&exif, Tag::FocalLength).map(|v| format!("{} mm", trim_num(v, 1))),
    );
    push(
        g,
        "等效焦距（35mm）",
        uint_of(&exif, Tag::FocalLengthIn35mmFilm)
            .filter(|&v| v > 0)
            .map(|v| format!("{v} mm")),
    );
    push(
        g,
        "最大光圈（APEX）",
        rational_of(&exif, Tag::MaxApertureValue).map(|v| format!("f/{:.1}", 2f64.powf(v / 2.0))),
    );
    push(
        g,
        "对焦距离",
        rational_of(&exif, Tag::SubjectDistance)
            .filter(|&v| v > 0.0)
            .map(|v| format!("{v:.2} m")),
    );
    push(
        g,
        "数码变焦",
        rational_of(&exif, Tag::DigitalZoomRatio)
            .filter(|&v| v > 0.0)
            .map(|v| format!("{v:.2}×")),
    );

    // ── 机身 ────────────────────────────────────────────────────────────────
    let g = "机身";
    push(g, "品牌", ascii_of(&exif, Tag::Make));
    push(g, "型号", ascii_of(&exif, Tag::Model));
    push(g, "机身序列号", ascii_of(&exif, Tag::BodySerialNumber));
    push(g, "固件 / 软件", ascii_of(&exif, Tag::Software));
    push(
        g,
        "EXIF 版本",
        exif.get_field(Tag::ExifVersion, In::PRIMARY)
            .map(|f| f.display_value().to_string()),
    );
    push(
        g,
        "感光方式",
        enum_of(&exif, Tag::SensingMethod, SENSING_METHOD),
    );

    // ── 图像 ────────────────────────────────────────────────────────────────
    let g = "图像";
    push(
        g,
        "方向",
        enum_of(&exif, Tag::Orientation, ORIENTATION).filter(|v| v != "正常"),
    );
    push(g, "色彩空间", enum_of(&exif, Tag::ColorSpace, COLOR_SPACE));
    push(
        g,
        "白平衡",
        enum_of(&exif, Tag::WhiteBalance, WHITE_BALANCE),
    );
    push(g, "光源", enum_of(&exif, Tag::LightSource, LIGHT_SOURCE));
    push(
        g,
        "场景类型",
        enum_of(&exif, Tag::SceneCaptureType, SCENE_CAPTURE),
    );
    push(g, "对比度", enum_of(&exif, Tag::Contrast, TONE));
    push(g, "饱和度", enum_of(&exif, Tag::Saturation, TONE));
    push(g, "锐度", enum_of(&exif, Tag::Sharpness, TONE));
    push(
        g,
        "增益控制",
        enum_of(&exif, Tag::GainControl, GAIN_CONTROL),
    );
    push(
        g,
        "后期处理",
        enum_of(&exif, Tag::CustomRendered, CUSTOM_RENDERED),
    );
    push(
        g,
        "主体距离范围",
        enum_of(&exif, Tag::SubjectDistanceRange, SUBJECT_DISTANCE_RANGE),
    );
    push(
        g,
        "像素尺寸",
        match (
            uint_of(&exif, Tag::PixelXDimension),
            uint_of(&exif, Tag::PixelYDimension),
        ) {
            (Some(w), Some(h)) => Some(format!("{w} × {h}")),
            (w, h) => w.or(h).map(|v| v.to_string()),
        },
    );
    push(g, "压缩方式", enum_of(&exif, Tag::Compression, COMPRESSION));
    push(
        g,
        "色彩模式",
        enum_of(&exif, Tag::PhotometricInterpretation, PHOTOMETRIC),
    );
    push(
        g,
        "每像素位数",
        exif.get_field(Tag::BitsPerSample, In::PRIMARY)
            .and_then(|f| {
                if let Value::Short(ref v) = f.value {
                    if !v.is_empty() {
                        return Some(
                            v.iter()
                                .map(|x| x.to_string())
                                .collect::<Vec<_>>()
                                .join(" / "),
                        );
                    }
                }
                None
            }),
    );
    push(
        g,
        "分辨率",
        match (
            rational_of(&exif, Tag::XResolution),
            enum_of(&exif, Tag::ResolutionUnit, RESOLUTION_UNIT),
        ) {
            (Some(x), unit) => Some(format!(
                "{} {}",
                trim_num(x, 0),
                unit.unwrap_or_else(|| "像素/单位".into())
            )),
            _ => None,
        },
    );

    // ── 时间 ────────────────────────────────────────────────────────────────
    let g = "时间";
    push(g, "拍摄时间", ascii_of(&exif, Tag::DateTimeOriginal));
    push(g, "数字化时间", ascii_of(&exif, Tag::DateTimeDigitized));
    push(g, "文件时间", ascii_of(&exif, Tag::DateTime));
    push(
        g,
        "时区偏移",
        ascii_of(&exif, Tag::OffsetTimeOriginal).or_else(|| ascii_of(&exif, Tag::OffsetTime)),
    );
    push(g, "亚秒", ascii_of(&exif, Tag::SubSecTimeOriginal));

    // ── 位置 ────────────────────────────────────────────────────────────────
    let g = "位置";
    push(g, "坐标", gps(&exif));
    push(g, "海拔", gps_altitude(&exif));

    // ── 说明 ────────────────────────────────────────────────────────────────
    let g = "说明";
    push(g, "图片描述", ascii_of(&exif, Tag::ImageDescription));
    push(g, "作者", ascii_of(&exif, Tag::Artist));
    push(g, "版权", ascii_of(&exif, Tag::Copyright));
    push(g, "备注", user_comment(&exif));

    Ok(out)
}

// ---------------------------------------------------------------------------
// 取值小工具
// ---------------------------------------------------------------------------

fn field<'a>(exif: &'a exif::Exif, tag: Tag) -> Option<&'a Field> {
    exif.get_field(tag, In::PRIMARY)
}

fn ascii_of(exif: &exif::Exif, tag: Tag) -> Option<String> {
    let f = field(exif, tag)?;
    if let Value::Ascii(ref v) = f.value {
        if let Some(bytes) = v.first() {
            let s: String = bytes
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as char)
                .collect();
            let s = s.trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

fn rational_of(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    match field(exif, tag)?.value {
        Value::Rational(ref v) => v.first().map(|r| r.to_f64()),
        Value::SRational(ref v) => v.first().map(|r| r.to_f64()),
        Value::Short(ref v) => v.first().map(|&x| x as f64),
        Value::Long(ref v) => v.first().map(|&x| x as f64),
        _ => None,
    }
}

fn uint_of(exif: &exif::Exif, tag: Tag) -> Option<u32> {
    field(exif, tag)?.value.get_uint(0)
}

/// 枚举型字段：查表翻译成人话，表里的编号对不上就退回原始数字。
fn enum_of(exif: &exif::Exif, tag: Tag, table: &[(u32, &str)]) -> Option<String> {
    let v = uint_of(exif, tag)?;
    Some(
        table
            .iter()
            .find(|(k, _)| *k == v)
            .map(|(_, s)| (*s).to_string())
            .unwrap_or_else(|| v.to_string()),
    )
}

/// UserComment 是 Undefined，头 8 字节是字符集标记（ASCII\0\0\0 等），正文在后面。
fn user_comment(exif: &exif::Exif) -> Option<String> {
    let f = field(exif, Tag::UserComment)?;
    let raw = match f.value {
        Value::Undefined(ref v, _) => v.clone(),
        Value::Byte(ref v) => v.clone(),
        _ => return None,
    };
    // 前 8 字节是编码标记，认不出来就整段当 ASCII 试试
    let body = if raw.len() > 8 { &raw[8..] } else { &raw[..] };
    let s: String = match &raw[..8.min(raw.len())] {
        b"UNICODE\0" => body
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]) as u32)
            .filter_map(char::from_u32)
            .collect(),
        _ => body.iter().map(|&b| b as char).collect(),
    };
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn gps(exif: &exif::Exif) -> Option<String> {
    let lat = dms(exif, Tag::GPSLatitude)?;
    let lon = dms(exif, Tag::GPSLongitude)?;
    let lat = match ascii_of(exif, Tag::GPSLatitudeRef)?.as_str() {
        s if s.eq_ignore_ascii_case("S") => -lat,
        _ => lat,
    };
    let lon = match ascii_of(exif, Tag::GPSLongitudeRef)?.as_str() {
        s if s.eq_ignore_ascii_case("W") => -lon,
        _ => lon,
    };
    Some(format!("{lat:.6}°, {lon:.6}°"))
}

/// 度分秒三个 Rational → 十进制度。
fn dms(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    if let Value::Rational(ref v) = field(exif, tag)?.value {
        if v.len() >= 3 {
            let d = v[0].to_f64();
            let m = v[1].to_f64();
            let s = v[2].to_f64();
            return Some(d + m / 60.0 + s / 3600.0);
        }
    }
    None
}

fn gps_altitude(exif: &exif::Exif) -> Option<String> {
    let v = rational_of(exif, Tag::GPSAltitude)?;
    let below = uint_of(exif, Tag::GPSAltitudeRef)
        .map(|r| r == 1)
        .unwrap_or(false);
    Some(format!("{}{:.1} m", if below { "-" } else { "" }, v))
}

/// 闪光灯是一个位域：是否闪、闪光模式、有没有防红眼都塞在一个 short 里。
fn flash(exif: &exif::Exif) -> Option<String> {
    let v = uint_of(exif, Tag::Flash)?;
    let mut parts: Vec<&str> = Vec::new();
    parts.push(if v & 1 == 1 { "已闪" } else { "未闪" });
    parts.push(match (v >> 3) & 0b11 {
        1 => "强制闪光",
        2 => "强制关闭",
        3 => "自动",
        _ => "",
    });
    if (v >> 6) & 1 == 1 {
        parts.push("防红眼");
    }
    let s: Vec<&str> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    Some(s.join(" · "))
}

/// 去掉多余的 0：35.0 → 35，1.500 → 1.5
fn trim_num(v: f64, max_digits: usize) -> String {
    let s = format!("{v:.max_digits$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// 枚举表
// ---------------------------------------------------------------------------

const EXPOSURE_PROGRAM: &[(u32, &str)] = &[
    (0, "未定义"),
    (1, "手动"),
    (2, "程序自动"),
    (3, "光圈优先"),
    (4, "快门优先"),
    (5, "创意（低速优先）"),
    (6, "运动（高速优先）"),
    (7, "人像"),
    (8, "风景"),
    (9, "微距"),
];

const EXPOSURE_MODE: &[(u32, &str)] = &[(0, "自动曝光"), (1, "手动曝光"), (2, "自动包围曝光")];

const METERING_MODE: &[(u32, &str)] = &[
    (0, "未知"),
    (1, "平均测光"),
    (2, "中央重点平均"),
    (3, "点测光"),
    (4, "多点测光"),
    (5, "评价测光（矩阵）"),
    (6, "局部测光"),
    (255, "其他"),
];

const ORIENTATION: &[(u32, &str)] = &[
    (1, "正常"),
    (2, "水平翻转"),
    (3, "旋转 180°"),
    (4, "垂直翻转"),
    (5, "顺时针 90° + 水平翻转"),
    (6, "顺时针 90°"),
    (7, "逆时针 90° + 水平翻转"),
    (8, "逆时针 90°"),
];

const COLOR_SPACE: &[(u32, &str)] = &[(1, "sRGB"), (2, "Adobe RGB"), (65535, "未校准")];

const WHITE_BALANCE: &[(u32, &str)] = &[(0, "自动"), (1, "手动")];

const LIGHT_SOURCE: &[(u32, &str)] = &[
    (0, "未知"),
    (1, "日光"),
    (2, "荧光灯"),
    (3, "白炽灯（钨丝）"),
    (4, "闪光灯"),
    (9, "晴天"),
    (10, "阴天"),
    (11, "阴影"),
    (15, "白荧光灯"),
    (17, "标准光源 A"),
    (18, "标准光源 B"),
    (19, "标准光源 C"),
    (20, "D55"),
    (21, "D65"),
    (22, "D75"),
    (255, "其他"),
];

const SCENE_CAPTURE: &[(u32, &str)] = &[(0, "标准"), (1, "人像"), (2, "风景"), (3, "夜景")];

const TONE: &[(u32, &str)] = &[(0, "标准"), (1, "低"), (2, "高")];

const GAIN_CONTROL: &[(u32, &str)] = &[
    (0, "无"),
    (1, "低增益提升"),
    (2, "高增益提升"),
    (3, "低增益降低"),
    (4, "高增益降低"),
];

const CUSTOM_RENDERED: &[(u32, &str)] = &[(0, "普通流程"), (1, "自定义处理")];

const SUBJECT_DISTANCE_RANGE: &[(u32, &str)] =
    &[(0, "未知"), (1, "微距"), (2, "近景"), (3, "远景")];

const SENSING_METHOD: &[(u32, &str)] = &[
    (1, "单色区传感器"),
    (2, "单芯片彩色"),
    (3, "双芯片彩色"),
    (4, "三芯片彩色"),
    (5, "色彩顺序"),
    (7, "三线性"),
    (8, "色彩线性"),
];

const COMPRESSION: &[(u32, &str)] = &[
    (1, "未压缩"),
    (6, "JPEG（旧式）"),
    (7, "JPEG"),
    (8, "Deflate"),
    (32773, "PackBits"),
    (34712, "JPEG 2000"),
    (34713, "尼康 NEF 压缩"),
    (34892, "Lossy JPEG"),
];

const PHOTOMETRIC: &[(u32, &str)] = &[
    (0, "WhiteIsZero"),
    (1, "BlackIsZero"),
    (2, "RGB"),
    (3, "调色板"),
    (5, "CMYK"),
    (6, "YCbCr"),
    (8, "CIELAB"),
    (32803, "彩色滤镜阵列（RAW）"),
    (34892, "Linear RAW"),
];

const RESOLUTION_UNIT: &[(u32, &str)] = &[(1, "像素/无单位"), (2, "像素/英寸"), (3, "像素/厘米")];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_zeros_but_keeps_digits() {
        assert_eq!(trim_num(35.0, 1), "35");
        assert_eq!(trim_num(1.5, 2), "1.5");
        assert_eq!(trim_num(0.333, 2), "0.33");
    }

    #[test]
    fn enum_lookup_falls_back_to_the_raw_number() {
        assert_eq!(lookup(&EXPOSURE_PROGRAM[..], 3), "光圈优先");
        assert_eq!(lookup(&EXPOSURE_PROGRAM[..], 77), "77");
    }

    fn lookup(table: &[(u32, &str)], v: u32) -> String {
        table
            .iter()
            .find(|(k, _)| *k == v)
            .map(|(_, s)| (*s).to_string())
            .unwrap_or_else(|| v.to_string())
    }

    // -----------------------------------------------------------------------
    // 下面这段手搓一个最小 TIFF（IFD0 挂 Make + Exif IFD 指针，Exif IFD 塞几个
    // 有代表性的 tag），用来在没有真实素材的情况下把「解析 + 翻译」整条链路跑通。
    // 真实素材的抽查走 thumb.rs 里那种 SP_SAMPLE 写法，这里不需要。
    // -----------------------------------------------------------------------

    const T_ASCII: u16 = 2;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_RATIONAL: u16 = 5;
    const T_SRATIONAL: u16 = 10;

    fn build_ifd(entries: &[(u16, u16, Vec<u8>)], self_offset: u32) -> Vec<u8> {
        let n = entries.len() as u16;
        let data_start = self_offset + 2 + 12 * n as u32 + 4;
        let mut out = Vec::new();
        out.extend_from_slice(&n.to_le_bytes());
        let mut data: Vec<u8> = Vec::new();
        for (tag, typ, val) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            let count = match *typ {
                T_SHORT => val.len() / 2,
                T_LONG => val.len() / 4,
                T_RATIONAL | T_SRATIONAL => val.len() / 8,
                _ => val.len(),
            } as u32;
            out.extend_from_slice(&count.to_le_bytes());
            if val.len() <= 4 {
                let mut v = val.clone();
                v.resize(4, 0);
                out.extend_from_slice(&v);
            } else {
                out.extend_from_slice(&(data_start + data.len() as u32).to_le_bytes());
                let mut v = val.clone();
                if v.len() % 2 == 1 {
                    v.push(0);
                }
                data.extend_from_slice(&v);
            }
        }
        out.extend_from_slice(&0u32.to_le_bytes()); // 没有下一个 IFD
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn parses_and_translates_a_minimal_tiff() {
        let make = b"NIKON CORPORATION\0".to_vec();
        let exif_offset = 8 + 2 + 12 * 2 + 4 + make.len() as u32;

        let exif_ifd = build_ifd(
            &[
                // 曝光程序 = 光圈优先
                (0x8822, T_SHORT, 3u16.to_le_bytes().to_vec()),
                // 测光模式 = 评价测光
                (0x9207, T_SHORT, 5u16.to_le_bytes().to_vec()),
                // 曝光补偿 = -1/3 EV
                (
                    0x9204,
                    T_SRATIONAL,
                    [(-1i32).to_le_bytes(), 3i32.to_le_bytes()].concat(),
                ),
                // 焦距 = 35mm，等效 52mm
                (
                    0x920a,
                    T_RATIONAL,
                    [35u32.to_le_bytes(), 1u32.to_le_bytes()].concat(),
                ),
                (0xa405, T_SHORT, 52u16.to_le_bytes().to_vec()),
                // 色彩空间 = sRGB
                (0xa001, T_SHORT, 1u16.to_le_bytes().to_vec()),
            ],
            exif_offset,
        );

        let ifd0 = build_ifd(
            &[
                (0x010f, T_ASCII, make.clone()),
                (0x8769, T_LONG, exif_offset.to_le_bytes().to_vec()),
            ],
            8,
        );

        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"II");
        buf.extend_from_slice(&42u16.to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes());
        buf.extend_from_slice(&ifd0);
        buf.extend_from_slice(&exif_ifd);

        let path = std::env::temp_dir().join("sp_exif_detail_test.tif");
        std::fs::write(&path, &buf).unwrap();

        let items = read_full(&path).expect("应能解析出自造的 TIFF");
        let get = |label: &str| {
            items
                .iter()
                .find(|i| i.label == label)
                .map(|i| i.value.clone())
                .unwrap_or_else(|| panic!("缺少 {label}：{items:?}"))
        };

        assert_eq!(get("品牌"), "NIKON CORPORATION");
        assert_eq!(get("曝光程序"), "光圈优先");
        assert_eq!(get("测光模式"), "评价测光（矩阵）");
        assert_eq!(get("曝光补偿"), "-0.33 EV");
        assert_eq!(get("焦距"), "35 mm");
        assert_eq!(get("等效焦距（35mm）"), "52 mm");
        assert_eq!(get("色彩空间"), "sRGB");
        // 分组顺序由后端定，曝光要排在镜头前面
        let order: Vec<&str> = items.iter().map(|i| i.group.as_str()).collect();
        assert!(order.contains(&"曝光") && order.contains(&"镜头"));

        let _ = std::fs::remove_file(&path);
    }
}
