//! NEF / JPG 配对。
//!
//! 每次快门产生两个文件（`DSC_0001.NEF` + `DSC_0001.JPG`）。
//! 如果按文件平铺，几千次快门会变成上万张卡片，同一张照片要判断两遍。
//!
//! 配对规则：**同目录 + 主文件名相同（大小写不敏感）+ 扩展名分属 RAW 组和图片组**。
//!
//! 注意 `photos` 表仍是「每个文件一行」，配对只通过 `pair_key` 聚合。
//! 这样天然兼容「某次只拍了 JPG」「某次只拍了 RAW」的混合情况，不需要特例代码。

use std::path::Path;

/// RAW 扩展名（含尼康 NEF / NRW，以及其他常见厂商格式）
const RAW_EXTS: &[&str] = &[
    "nef", "nrw", // Nikon
    "cr2", "cr3", "crw", // Canon
    "arw", "srf", "sr2", // Sony
    "raf", // Fujifilm
    "orf", // Olympus / OM
    "rw2", // Panasonic
    "pef", // Pentax
    "srw", // Samsung
    "raw", "rwl", // Leica
    "dng", // Adobe / 通用
    "3fr", // Hasselblad
    "iiq", // Phase One
];

const JPEG_EXTS: &[&str] = &["jpg", "jpeg", "jpe"];

const OTHER_EXTS: &[&str] = &["heic", "heif", "tif", "tiff", "png", "avif"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Raw,
    Jpeg,
    Other,
}

impl FileKind {
    /// 存库用的字符串标识。
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Raw => "raw",
            FileKind::Jpeg => "jpeg",
            FileKind::Other => "other",
        }
    }
}

/// 按扩展名判断文件类型；无法识别的返回 `None`（扫描时直接跳过）。
pub fn classify(ext: &str) -> Option<FileKind> {
    let e = ext.to_ascii_lowercase();
    if RAW_EXTS.contains(&e.as_str()) {
        Some(FileKind::Raw)
    } else if JPEG_EXTS.contains(&e.as_str()) {
        Some(FileKind::Jpeg)
    } else if OTHER_EXTS.contains(&e.as_str()) {
        Some(FileKind::Other)
    } else {
        None
    }
}

/// 生成配对键：`规范化目录（小写）/ 主文件名（小写）`。
///
/// 目录和文件名都转小写，这样 `.NEF` 与 `.nef`、`DSC_0001` 与 `dsc_0001` 都能配上。
/// 路径分隔符保持平台原生（数据库是每台机器各自的，不需要跨平台统一）。
pub fn pair_key(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.to_ascii_lowercase();
    let parent = path.parent()?.to_string_lossy().to_ascii_lowercase();
    let parent = parent.trim_end_matches(['/', '\\']);
    Some(format!("{parent}/{stem}"))
}

/// 文件扩展名（小写）。
pub fn ext_lower(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn nef_and_jpg_share_a_pair_key() {
        let dir = PathBuf::from("/photos/2026-shoot");
        let a = pair_key(&dir.join("DSC_0001.NEF")).unwrap();
        let b = pair_key(&dir.join("DSC_0001.JPG")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn pair_key_is_case_insensitive() {
        let dir = PathBuf::from("/photos/Shoot");
        let a = pair_key(&dir.join("DSC_0001.nef")).unwrap();
        let b = pair_key(&dir.join("dsc_0001.NEF")).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_shots_do_not_collide() {
        let dir = PathBuf::from("/photos/2026-shoot");
        assert_ne!(
            pair_key(&dir.join("DSC_0001.NEF")).unwrap(),
            pair_key(&dir.join("DSC_0002.NEF")).unwrap()
        );
    }

    #[test]
    fn same_name_in_different_dirs_do_not_collide() {
        assert_ne!(
            pair_key(&PathBuf::from("/a/DSC_0001.NEF")).unwrap(),
            pair_key(&PathBuf::from("/b/DSC_0001.NEF")).unwrap()
        );
    }

    #[test]
    fn classify_knows_nikon_formats() {
        assert_eq!(classify("NEF"), Some(FileKind::Raw));
        assert_eq!(classify("nrw"), Some(FileKind::Raw));
        assert_eq!(classify("JPG"), Some(FileKind::Jpeg));
        assert_eq!(classify("jpeg"), Some(FileKind::Jpeg));
        assert!(classify("txt").is_none());
    }
}
