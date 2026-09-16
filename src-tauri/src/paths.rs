//! 跨平台数据目录定位。
//!
//! 约定：数据库与缩略图缓存都放在系统规范的用户数据目录下，
//! **原片永远不进这里**——原片在哪就在哪，系统只记路径和指纹。

use anyhow::{anyhow, Result};
use std::path::PathBuf;

const APP_DIR: &str = "SP";

/// 返回应用数据目录，不存在则创建。
pub fn data_dir() -> Result<PathBuf> {
    let base: PathBuf = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("找不到 APPDATA 环境变量"))?
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library").join("Application Support"))
            .ok_or_else(|| anyhow!("找不到 HOME 环境变量"))?
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
            .ok_or_else(|| anyhow!("找不到可用的数据目录"))?
    };

    let dir = base.join(APP_DIR);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 数据库文件路径。
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("library.db"))
}

/// 缩略图缓存根目录（按哈希前两位分桶存放）。
#[allow(dead_code)]
pub fn thumbs_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("thumbs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
