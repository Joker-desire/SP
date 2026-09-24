//! Tauri 后端入口。
//!
//! 安全边界的核心约定：**前端（WebView）无法直接访问文件系统**，
//! 所有磁盘操作必须经过这里显式暴露的命令。这正是「原片只读」原则的机制保障
//! —— 前端就算有 bug 也碰不到你的照片。

mod af;
mod analyze;
mod blink;
mod db;
mod exif_detail;
mod indexer;
mod pairing;
mod paths;
mod thumb;

use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager};

pub struct AppState {
    pub db: Arc<Mutex<Connection>>,
    /// 启动时数据库没打开成功的话，这里记着原因，前端会把它显示成一条醒目的提示。
    pub startup_error: Option<String>,
}

/// 启动是否出了问题。前端一进来就问一次。
#[tauri::command]
fn startup_status(state: tauri::State<'_, AppState>) -> Option<String> {
    state.startup_error.clone()
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 扫描一个目录，建索引并完成 NEF / JPG 配对。
///
/// 走 spawn_blocking，避免几千张的解析卡住界面。
///
/// 进度是**边扫边推**的：首次扫几千张要跑几分钟，没有反馈就是在让用户
/// 对着一个不知道死没死的窗口发呆。
///
/// `include_dirs` 是「只扫这些子目录」时的勾选结果（绝对路径）。为空或不传，
/// 就扫描整个 `path`（递归）。分批写库让前端能在扫描进行中就开始显示已索引的部分。
#[tauri::command]
async fn scan_folder(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    include_dirs: Option<Vec<String>>,
) -> Result<indexer::ScanSummary, String> {
    let db = state.db.clone();
    // includeDirs 的三种取值各有含义，别合并：
    //   None      → 递归整个根目录（启动时自动重扫、没有子目录可勾时的默认行为）
    //   Some([])  → 只扫根目录自己那一层：文件夹里既有照片又有子文件夹，
    //               用户一个子文件夹都不勾时就是这个意思
    //   Some(dirs)→ 只扫勾选的这些子目录
    let (roots, max_depth): (Vec<PathBuf>, Option<usize>) = match include_dirs {
        Some(d) if !d.is_empty() => (d.into_iter().map(PathBuf::from).collect(), None),
        Some(_) => (vec![PathBuf::from(&path)], Some(1)),
        None => (vec![PathBuf::from(&path)], None),
    };
    tauri::async_runtime::spawn_blocking(move || {
        indexer::scan_with_progress(
            &roots,
            &db,
            |p| {
                // 推送失败不是错误：窗口已经关了而已，扫描本身该继续跑完
                let _ = app.emit("scan://progress", &p);
            },
            max_depth,
        )
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 把「还没分析过」的照片算一遍清晰度和曝光。
///
/// 扫描结束之后由前端自行触发，不挂在扫描里——扫描的KPI是快点出图，
/// 分析要解码，混在一起会把首次导入拖成干等。
#[tauri::command]
async fn analyze_library(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    roots: Option<Vec<String>>,
) -> Result<analyze::AnalyzeSummary, String> {
    let db = state.db.clone();
    let roots: Vec<PathBuf> = roots
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    tauri::async_runtime::spawn_blocking(move || {
        analyze::analyze_pending(&roots, &db, |p| {
            let _ = app.emit("analyze://progress", &p);
        })
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 闭眼检测的总开关存在库里（meta 表），不在前端。
///
/// 存库里的理由：它决定的是「扫描之后要不要多跑一趟」，属于这台机器上的
/// 持久偏好，跟窗口大小那种界面状态不是一回事——换台机器不该被带过去。
const BLINK_KEY: &str = "blink_enabled";

#[tauri::command]
async fn blink_enabled(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        Ok(db::meta_get(&conn, BLINK_KEY)
            .map_err(|e| format!("{e:#}"))?
            .map(|v| v == "1")
            .unwrap_or(false))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn set_blink_enabled(state: tauri::State<'_, AppState>, on: bool) -> Result<(), String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        db::meta_set(&conn, BLINK_KEY, if on { "1" } else { "0" }).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 把「还没检测过」的照片挨个找一遍有没有人闭眼。
///
/// 和画面分析一样是后台趟：分批、可中断、算过的不再算。
/// 开关关着时前端不会调它——几千张的解码 + 推理不是免费的。
#[tauri::command]
async fn analyze_blink(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    roots: Option<Vec<String>>,
) -> Result<blink::BlinkSummary, String> {
    let db = state.db.clone();
    let roots: Vec<PathBuf> = roots
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    tauri::async_runtime::spawn_blocking(move || {
        blink::analyze_pending(&roots, &db, |p| {
            let _ = app.emit("blink://progress", &p);
        })
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 中途喊停闭眼检测。已经算完的照常留着，剩下的下次接着算。
#[tauri::command]
fn cancel_blink() {
    blink::cancel();
}

/// 单张现算：大图里按一下就出结果，不用等整库跑完。
///
/// 算完顺手写回库里——下次批量检测会跳过它，批量跑到一半被打断也不会白算。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BlinkReport {
    faces: usize,
    ratio: Option<f64>,
    closed: bool,
}

#[tauri::command]
async fn photo_blink(state: tauri::State<'_, AppState>, id: i64) -> Result<BlinkReport, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path: String = {
            let conn = db.lock().map_err(|e| e.to_string())?;
            conn.query_row("SELECT path FROM photos WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|e| format!("这张照片已经不在库里了：{e}"))?
        };

        let report = blink::eyes_for(Path::new(&path)).map_err(|e| format!("{e:#}"))?;

        {
            let conn = db.lock().map_err(|e| e.to_string())?;
            conn.execute(
                "UPDATE photos SET faces = ?1, eye_ratio = ?2 WHERE id = ?3",
                rusqlite::params![report.faces as i64, report.ratio, id],
            )
            .map_err(|e| format!("写回检测结果失败：{e}"))?;
        }

        Ok(BlinkReport {
            faces: report.faces,
            ratio: report.ratio,
            closed: report.closed(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 列出某个目录下的子目录（含自身），供「文件夹范围选择」弹窗做一棵可勾选的树。
#[tauri::command]
async fn list_subdirs(path: String) -> Result<Vec<indexer::DirNode>, String> {
    Ok(indexer::list_subdirs(Path::new(&path), 4, 3000))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryStats {
    files: i64,
    pairs: i64,
    orphan_raw: i64,
    orphan_jpg: i64,
    exif_failed: i64,
    cameras: i64,
    db_path: String,
}

#[tauri::command]
async fn library_stats(state: tauri::State<'_, AppState>) -> Result<LibraryStats, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        stats_of(&conn).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn stats_of(conn: &Connection) -> anyhow::Result<LibraryStats> {
    let files: i64 = conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?;

    let (pairs, orphan_raw, orphan_jpg) = conn.query_row(
        "SELECT COUNT(*),
                SUM(CASE WHEN cnt = 1 AND has_raw = 1 THEN 1 ELSE 0 END),
                SUM(CASE WHEN cnt = 1 AND has_jpg = 1 THEN 1 ELSE 0 END)
         FROM (
            SELECT pair_key, COUNT(*) AS cnt,
                   MAX(CASE WHEN file_kind = 'raw'  THEN 1 ELSE 0 END) AS has_raw,
                   MAX(CASE WHEN file_kind = 'jpeg' THEN 1 ELSE 0 END) AS has_jpg
            FROM photos GROUP BY pair_key
         )",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        },
    )?;

    let exif_failed: i64 =
        conn.query_row("SELECT COUNT(*) FROM photos WHERE exif_ok = 0", [], |r| {
            r.get(0)
        })?;

    let cameras: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT camera_serial) FROM photos WHERE camera_serial IS NOT NULL",
        [],
        |r| r.get(0),
    )?;

    Ok(LibraryStats {
        files,
        pairs,
        orphan_raw,
        orphan_jpg,
        exif_failed,
        cameras,
        db_path: paths::db_path()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// 列表与筛选
//
// 「按拍摄日期」是选片时最常用的维度——一次只处理一场拍摄，所以日期不该是
// 可选功能，而该是主入口。这里把它和「文件类型」「机身」一起做成左侧栏的分面。
// ---------------------------------------------------------------------------

/// 时间来源有两种语义，必须分开处理，否则会出现「晚上的照片被算到第二天」：
/// - `taken_at_corrected`：来自 EXIF，存的是相机当地的墙上时间（按 UTC 解读），
///   所以按 UTC 格式化就能原样还原出「拍摄当天」。
/// - `mtime`：文件修改时间，是真实时刻，要按本机时区换算成本地日期。
const DAY_KEY: &str = "CASE WHEN p.taken_at_corrected IS NOT NULL \
     THEN strftime('%Y-%m-%d', p.taken_at_corrected, 'unixepoch') \
     ELSE strftime('%Y-%m-%d', p.mtime, 'unixepoch', 'localtime') END";

/// 同样的语义，输出「年-月-日 时:分:秒」用于显示。
const TIME_TEXT: &str = "CASE WHEN p.taken_at_corrected IS NOT NULL \
     THEN strftime('%Y-%m-%d %H:%M:%S', p.taken_at_corrected, 'unixepoch') \
     ELSE strftime('%Y-%m-%d %H:%M:%S', p.mtime, 'unixepoch', 'localtime') END";

const HAS_RAW: &str = "EXISTS(SELECT 1 FROM photos a \
     WHERE a.pair_key = p.pair_key AND a.file_kind = 'raw')";
const HAS_JPG: &str = "EXISTS(SELECT 1 FROM photos b \
     WHERE b.pair_key = p.pair_key AND b.file_kind = 'jpeg')";

/// 选片结果的两列。`decisions` 里没有对应行＝还没看过，所以必须用 COALESCE
/// 给出默认值，否则「未标记」这一档会因为 NULL 而筛不出来。
const DECISION: &str = "COALESCE(d.decision, 'none')";
const STARS: &str = "COALESCE(d.stars, 0)";
/// 色标。没打过色标的行取不到值，同样要兜成空串，否则「无色标」这一档筛不出来。
const COLOR: &str = "COALESCE(d.color, '')";

/// 色标的全部取值与显示名。顺序决定 `6`-`0` 这五个键的排布，
/// 前端 `src/main.ts` 里有一份对应的定义，两边改任一边都要通知另一边。
const COLOR_KEYS: &[(&str, &str)] = &[
    ("red", "红"),
    ("yellow", "黄"),
    ("green", "绿"),
    ("blue", "蓝"),
    ("purple", "紫"),
];

/// 认不出来的一律当无色标，而不是报错——色标是给人眼帮忙的，
/// 不该因为一个脏值让这一张照片没法选。
fn normalize_color(s: Option<&str>) -> Option<String> {
    match s?.trim().to_ascii_lowercase().as_str() {
        "red" => Some("red".into()),
        "yellow" => Some("yellow".into()),
        "green" => Some("green".into()),
        "blue" => Some("blue".into()),
        "purple" => Some("purple".into()),
        _ => None,
    }
}

/// `build_where` 拼出来的条件会引用别名 `p` 和 `d`，
/// 所有用它的查询都必须带上这个 FROM，否则会漏掉选片状态这一维的筛选。
const FROM_PHOTOS: &str = "FROM photos p LEFT JOIN decisions d ON d.pair_key = p.pair_key";

/// 表示「这个维度上没有值」的哨兵键（读不到机身序列号 / 没有拍摄时间）。
const NONE_KEY: &str = "__none__";

/// 前端传来的筛选条件，全部可选——不传就是不筛。
#[derive(Debug, Default, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairFilter {
    /// all / both / rawOnly / jpgOnly / orphan
    #[serde(default)]
    pair_state: Option<String>,
    /// 机身序列号，或 "__none__"
    #[serde(default)]
    camera_serial: Option<String>,
    /// YYYY-MM-DD，或 "__none__"
    #[serde(default)]
    day: Option<String>,
    /// 文件名关键词
    #[serde(default)]
    search: Option<String>,
    /// 选片状态：none（未标记）/ keep / reject / marked（已标记，不分保留淘汰）
    #[serde(default)]
    decision: Option<String>,
    /// 星级。`None`＝不筛；`Some(0)`＝只要没打星的。两者必须分开——
    /// 合成一个「0 就是不筛」的约定，会让「找没打星的」这个需求没法表达。
    #[serde(default)]
    stars: Option<i64>,
    /// 色标：red / yellow / green / blue / purple。`None`＝不筛。
    #[serde(default)]
    color: Option<String>,
    /// takenDesc（默认）/ takenAsc / nameAsc / nameDesc / sizeDesc / starsDesc
    #[serde(default)]
    sort: Option<String>,
    /// 只看这些目录下的照片（绝对路径）。`None`／空＝不限，查整个图库。
    ///
    /// 换文件夹时旧文件夹的记录**故意留在库里**（选片标记是按 pair_key 存的，
    /// 留住它们，回头再选同一个文件夹时标记会自己回来），所以视图必须靠这个
    /// 条件限定在「当前选中的文件夹」上，否则换完文件夹会看到上次的照片还在。
    #[serde(default)]
    roots: Option<Vec<String>>,
    /// 只要这几张（主文件的 photo id）。用来导出「当前选中的那几张」——
    /// 不靠筛选条件兜圈子，勾了什么就导出什么。
    #[serde(default)]
    ids: Option<Vec<i64>>,
    /// 画面质量：blur（可能糊了）/ over（高光溢出）/ under（暗部死黑）。
    /// 门槛在 analyze.rs 里，前后端共用同一套常量。
    #[serde(default)]
    quality: Option<String>,
    /// 镜头型号，或 "__none__"
    #[serde(default)]
    lens: Option<String>,
    /// 焦段分档：wide / normal / tele / super
    #[serde(default)]
    focal: Option<String>,
    /// ISO 分档：low / mid / high / veryHigh
    #[serde(default)]
    iso: Option<String>,
}

/// 网格用的照片卡片。以「一次快门」为单位，而不是以文件为单位。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PairCard {
    /// 主文件的 photo id —— 前端拿它去请求缩略图
    id: i64,
    pair_key: String,
    path: String,
    file_kind: String,
    /// both / rawOnly / jpgOnly
    pair_state: String,
    /// 已格式化好的拍摄时刻（后端做，避免前端按本机时区重新解释导致偏移）
    taken_at_text: Option<String>,
    /// YYYY-MM-DD，用于按日筛选与分组
    day_key: Option<String>,
    camera_model: Option<String>,
    camera_serial: Option<String>,
    lens: Option<String>,
    focal_len: Option<f64>,
    aperture: Option<f64>,
    shutter: Option<String>,
    iso: Option<i64>,
    file_size: i64,
    /// 实际走的解码路径（生成过缩略图后才有值）
    decode_path: Option<String>,
    /// none / keep / reject —— 选片结果，跟着卡片一起下发，前端不用再问一次
    decision: String,
    /// 0–5
    stars: i64,
    /// 色标：'' / red / yellow / green / blue / purple
    color: String,
    /// 画面分析。NULL = 还没分析过——后台分析是扫描之后才跑的，
    /// 刚扫完的库里大部分都是 NULL，前端要能正常显示而不是当成 0。
    sharpness: Option<f64>,
    /// 高光溢出像素占比 0–1
    overexposed: Option<f64>,
    /// 暗部死黑像素占比 0–1
    underexposed: Option<f64>,
    /// 检出的人脸数。NULL = 还没检测过（闭眼检测默认关，多数时候就是 NULL）。
    faces: Option<i64>,
    /// 最闭的那只眼睛的 EAR；NULL = 没测出关键点（包括压根没脸的情况）。
    eye_ratio: Option<f64>,
}

/// 一页照片 + 满足条件的总数（前端据此显示「共 N 张」和决定还要不要继续加载）。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PairPage {
    items: Vec<PairCard>,
    total: i64,
}

/// 分面计数，用于左侧筛选栏。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Facet {
    key: String,
    label: String,
    count: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LibraryFacets {
    total: i64,
    /// 已过目：有保留 / 淘汰决定，或打了星。用于「还剩多少没看」的进度。
    processed: i64,
    /// 其中判为保留的张数
    kept: i64,
    pair_states: Vec<Facet>,
    cameras: Vec<Facet>,
    days: Vec<Facet>,
    /// 日期列表是否被截断（超过上限时只返回最近的若干天）
    days_truncated: bool,
    /// 选片状态：全部 / 未标记 / 保留 / 淘汰
    decisions: Vec<Facet>,
    /// 星级：未打星 / 1★…5★
    stars: Vec<Facet>,
    /// 色标：红 / 黄 / 绿 / 蓝 / 紫。顺序与 `COLOR_KEYS` 一致。
    colors: Vec<Facet>,
    /// 画面质量：可能糊了 / 高光溢出 / 暗部死黑。分析还没跑完时计数偏小，
    /// 这是正常的——跑完一趟再打开侧栏就补齐了。
    quality: Vec<Facet>,
    /// 镜头型号（有几种列几种）
    lenses: Vec<Facet>,
    /// 焦段分档：wide / normal / tele / super
    focals: Vec<Facet>,
    /// ISO 分档：low / mid / high / veryHigh
    isos: Vec<Facet>,
}

/// 搜索关键词里的 LIKE 通配符要转义，否则输入一个 `%` 会把整个库匹配出来。
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 目录前缀。末尾补上分隔符，这样 `/a/b` 不会把兄弟目录 `/a/bc` 也算进来。
fn dir_prefix(dir: &str) -> String {
    let trimmed = dir.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return String::new();
    }
    format!("{}{}", trimmed, std::path::MAIN_SEPARATOR)
}

/// 目录范围条件：`substr(p.path, 1, length(?)) = ?`。
///
/// 长度交给 SQLite 自己算（`length()` 按字符数，中文目录名也不会错位），
/// 比在 Rust 里取字节长度再传进去稳。返回空串表示「不限」。
fn scope_group(roots: Option<&[String]>) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;

    let roots = match roots {
        Some(r) if !r.is_empty() => r,
        _ => return (String::new(), Vec::new()),
    };

    let mut parts: Vec<String> = Vec::new();
    let mut args: Vec<Value> = Vec::new();
    for r in roots {
        let prefix = dir_prefix(r);
        if prefix.is_empty() {
            continue;
        }
        parts.push("substr(p.path, 1, length(?)) = ?".to_string());
        // 同一个前缀要绑两次：一次给 length()，一次给比较
        args.push(Value::Text(prefix.clone()));
        args.push(Value::Text(prefix));
    }

    if parts.is_empty() {
        (String::new(), Vec::new())
    } else {
        (format!("({})", parts.join(" OR ")), args)
    }
}

/// 拼在 `WHERE` 后面的版本：不限时是空串，方便直接追加。
fn scope_where(roots: Option<&[String]>) -> (String, Vec<rusqlite::types::Value>) {
    let (group, args) = scope_group(roots);
    if group.is_empty() {
        (String::new(), args)
    } else {
        (format!(" AND {group}"), args)
    }
}

fn build_where(f: &PairFilter) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;

    let mut conds: Vec<String> = vec!["p.is_primary = 1".to_string()];
    let mut args: Vec<Value> = Vec::new();

    // 显式指定的一组照片，优先级最高：勾了什么就是什么，不再叠加目录范围之外的判断
    if let Some(ids) = f.ids.as_deref().filter(|v| !v.is_empty()) {
        // SQLite 的变量数上限远大于一次选片的张数，这里不切块
        let holders = vec!["?"; ids.len()].join(",");
        conds.push(format!("p.id IN ({holders})"));
        for id in ids {
            args.push(Value::Integer(*id));
        }
    }

    // 目录范围。参数按 conds 的先后依次 push，顺序对得上就行。
    let (scope, scope_args) = scope_group(f.roots.as_deref());
    if !scope.is_empty() {
        conds.push(scope);
        args.extend(scope_args);
    }

    match f.pair_state.as_deref().unwrap_or("all") {
        "both" => conds.push(format!("({HAS_RAW} AND {HAS_JPG})")),
        "rawOnly" => conds.push(format!("({HAS_RAW} AND NOT {HAS_JPG})")),
        "jpgOnly" => conds.push(format!("(NOT {HAS_RAW} AND {HAS_JPG})")),
        "orphan" => conds.push(format!("NOT ({HAS_RAW} AND {HAS_JPG})")),
        _ => {}
    }

    if let Some(cam) = f.camera_serial.as_deref().filter(|s| !s.is_empty()) {
        if cam == NONE_KEY {
            conds.push("p.camera_serial IS NULL".to_string());
        } else {
            conds.push("p.camera_serial = ?".to_string());
            args.push(Value::Text(cam.to_string()));
        }
    }

    if let Some(day) = f.day.as_deref().filter(|s| !s.is_empty()) {
        if day == NONE_KEY {
            conds.push(format!("({DAY_KEY}) IS NULL"));
        } else {
            conds.push(format!("({DAY_KEY}) = ?"));
            args.push(Value::Text(day.to_string()));
        }
    }

    if let Some(q) = f.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        conds.push("p.path LIKE ? ESCAPE '\\'".to_string());
        args.push(Value::Text(format!("%{}%", escape_like(q))));
    }

    // 选片状态。这里的取值是白名单——别的字符串一律当「不筛」，
    // 免得前端传个笔误就把条件悄悄丢掉（结果看起来「筛了但没生效」）。
    match f.decision.as_deref() {
        Some("none") => conds.push(format!("{DECISION} = 'none'")),
        Some("keep") => conds.push(format!("{DECISION} = 'keep'")),
        Some("reject") => conds.push(format!("{DECISION} = 'reject'")),
        Some("marked") => conds.push(format!("{DECISION} <> 'none'")),
        _ => {}
    }

    if let Some(s) = f.stars {
        conds.push(format!("{STARS} = ?"));
        args.push(Value::Integer(s.clamp(0, 5)));
    }

    // 色标。认不出来的值按「不筛」处理，理由同 decision：
    // 传错一个字符串导致条件被悄悄丢掉，比明确报错更难查。
    if let Some(c) = normalize_color(f.color.as_deref()) {
        conds.push(format!("{COLOR} = ?"));
        args.push(Value::Text(c));
    }

    if let Some(l) = f.lens.as_deref().filter(|s| !s.is_empty()) {
        if l == NONE_KEY {
            conds.push("p.lens IS NULL".to_string());
        } else {
            conds.push("p.lens = ?".to_string());
            args.push(Value::Text(l.to_string()));
        }
    }
    if let Some((lo, hi)) = focal_range(f.focal.as_deref()) {
        conds.push("p.focal_len >= ? AND p.focal_len < ?".to_string());
        args.push(Value::Real(lo));
        args.push(Value::Real(hi));
    }
    if let Some((lo, hi)) = iso_range(f.iso.as_deref()) {
        conds.push("p.iso >= ? AND p.iso < ?".to_string());
        args.push(Value::Integer(lo));
        args.push(Value::Integer(hi));
    }

    // 画面质量。没分析过的照片（sharpness IS NULL）一律不算「有问题」——
    // 分析是后台跑的，刚扫完就筛会把还没轮到的照片全列进「糊了」，那是误报。
    match f.quality.as_deref() {
        Some("blur") => conds.push(format!(
            "p.sharpness IS NOT NULL AND p.sharpness < {}",
            analyze::BLUR_THRESHOLD
        )),
        Some("over") => conds.push(format!(
            "p.overexposed IS NOT NULL AND p.overexposed >= {}",
            analyze::OVEREXPOSED_THRESHOLD
        )),
        Some("under") => conds.push(format!(
            "p.underexposed IS NOT NULL AND p.underexposed >= {}",
            analyze::UNDEREXPOSED_THRESHOLD
        )),
        // 疑似闭眼。同理：没检测过的一律不算有嫌疑。
        Some("blink") => conds.push(format!(
            "p.eye_ratio IS NOT NULL AND p.eye_ratio < {}",
            blink::CLOSED_THRESHOLD
        )),
        _ => {}
    }

    (conds.join(" AND "), args)
}

/// 焦段分档。档位而不是自由区间：选片时想的是「这批广角」「那批长焦」，
/// 让人填 24–70 反而多一步。上界取开区间，相邻档不会重叠漏张。
fn focal_range(key: Option<&str>) -> Option<(f64, f64)> {
    match key.unwrap_or("") {
        "wide" => Some((0.0, 24.0)),
        "normal" => Some((24.0, 70.0)),
        "tele" => Some((70.0, 200.0)),
        "super" => Some((200.0, f64::MAX)),
        _ => None,
    }
}

/// ISO 分档。边界按相机实际的档位跳变来切（400 / 1600 / 6400）。
fn iso_range(key: Option<&str>) -> Option<(i64, i64)> {
    match key.unwrap_or("") {
        "low" => Some((0, 400)),
        "mid" => Some((400, 1600)),
        "high" => Some((1600, 6400)),
        "veryHigh" => Some((6400, i64::MAX)),
        _ => None,
    }
}

fn order_by(sort: Option<&str>) -> &'static str {
    match sort.unwrap_or("takenDesc") {
        "takenAsc" => "ORDER BY COALESCE(p.taken_at_corrected, p.mtime) ASC, p.path",
        "nameAsc" => "ORDER BY p.path ASC",
        "nameDesc" => "ORDER BY p.path DESC",
        "sizeDesc" => "ORDER BY p.file_size DESC, p.path",
        "starsDesc" => "ORDER BY COALESCE(d.stars, 0) DESC, p.path",
        // 清晰度：低→高排在最前，拍糊的自然浮到前面来
        "sharpAsc" => "ORDER BY (p.sharpness IS NULL), p.sharpness ASC, p.path",
        "sharpDesc" => "ORDER BY (p.sharpness IS NULL), p.sharpness DESC, p.path",
        _ => "ORDER BY COALESCE(p.taken_at_corrected, p.mtime) DESC, p.path",
    }
}

/// 按筛选条件取一页照片。
///
/// 用 `LIMIT / OFFSET` 分页而不是一次全取：几千张的元数据一次塞给前端，
/// 光是 IPC 序列化就要几百毫秒，而用户第一眼只看得见二三十张。
#[tauri::command]
async fn list_pairs(
    state: tauri::State<'_, AppState>,
    filter: Option<PairFilter>,
    limit: i64,
    offset: i64,
) -> Result<PairPage, String> {
    let db = state.db.clone();
    let filter = filter.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        query_pairs(&conn, &filter, limit, offset).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn query_pairs(
    conn: &Connection,
    filter: &PairFilter,
    limit: i64,
    offset: i64,
) -> anyhow::Result<PairPage> {
    let (where_sql, mut args) = build_where(filter);

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) {FROM_PHOTOS} WHERE {where_sql}"),
        rusqlite::params_from_iter(args.iter()),
        |r| r.get(0),
    )?;

    let sql = format!(
        "SELECT p.id, p.pair_key, p.path, p.file_kind,
                (SELECT MAX(CASE WHEN q.file_kind = 'raw'  THEN 1 ELSE 0 END)
                   FROM photos q WHERE q.pair_key = p.pair_key),
                (SELECT MAX(CASE WHEN q.file_kind = 'jpeg' THEN 1 ELSE 0 END)
                   FROM photos q WHERE q.pair_key = p.pair_key),
                {TIME_TEXT}, {DAY_KEY},
                p.camera_model, p.camera_serial,
                p.lens, p.focal_len, p.aperture, p.shutter, p.iso,
                p.file_size, p.decode_path,
                {DECISION}, {STARS}, {COLOR},
                p.sharpness, p.overexposed, p.underexposed, p.faces, p.eye_ratio
         {FROM_PHOTOS}
         WHERE {where_sql}
         {}
         LIMIT ? OFFSET ?",
        order_by(filter.sort.as_deref())
    );

    args.push(rusqlite::types::Value::Integer(limit));
    args.push(rusqlite::types::Value::Integer(offset));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        let has_raw: Option<i64> = r.get(4)?;
        let has_jpg: Option<i64> = r.get(5)?;
        let pair_state = match (has_raw.unwrap_or(0), has_jpg.unwrap_or(0)) {
            (1, 1) => "both",
            (1, 0) => "rawOnly",
            _ => "jpgOnly",
        };
        Ok(PairCard {
            id: r.get(0)?,
            pair_key: r.get(1)?,
            path: r.get(2)?,
            file_kind: r.get(3)?,
            pair_state: pair_state.to_string(),
            taken_at_text: r.get(6)?,
            day_key: r.get(7)?,
            camera_model: r.get(8)?,
            camera_serial: r.get(9)?,
            lens: r.get(10)?,
            focal_len: r.get(11)?,
            aperture: r.get(12)?,
            shutter: r.get(13)?,
            iso: r.get(14)?,
            file_size: r.get(15)?,
            decode_path: r.get(16)?,
            decision: r.get(17)?,
            stars: r.get(18)?,
            color: r.get(19)?,
            sharpness: r.get(20)?,
            overexposed: r.get(21)?,
            underexposed: r.get(22)?,
            faces: r.get(23)?,
            eye_ratio: r.get(24)?,
        })
    })?;

    let mut items = Vec::new();
    for row in rows {
        items.push(row?);
    }
    Ok(PairPage { items, total })
}

/// 左侧筛选栏的计数。
///
/// 计数是**整个图库**的口径（不是「在当前筛选结果里再统计」），
/// 这样点开筛选栏时看到的数字始终稳定，不会因为叠加条件而变小到看不懂。
#[tauri::command]
async fn library_facets(
    state: tauri::State<'_, AppState>,
    roots: Option<Vec<String>>,
) -> Result<LibraryFacets, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        facets_of(&conn, roots.as_deref()).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 日期列表的上限。超过就只给最近的若干天——一次选片不会翻到三年前。
const DAY_FACET_LIMIT: i64 = 400;

// ── 照片详情 ─────────────────────────────────────────────────────────────
//
// 大图查看时的「详细信息」面板。网格卡片只摆关键参数，这里把库里有的
// 全部摆出来：文件、EXIF、画面分析、库内指纹，一次问齐，翻页时重查。

/// 同一次快门的另一半文件（NEF ↔ JPG）。`exists` 现查现答——
/// 原片可能已经被挪走，库存记录不代表文件还在。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SiblingFile {
    id: i64,
    path: String,
    file_kind: String,
    file_size: i64,
    exists: bool,
}

/// 单张照片的完整档案。时间一律返回现成的文本（和侧栏 / 底栏同一套语义），
/// 前端不再自己拿时间戳换算，免得时区口径出现第二套。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PhotoDetail {
    id: i64,
    path: String,
    file_name: String,
    dir: String,
    pair_key: String,
    file_kind: String,
    is_primary: bool,
    file_size: i64,
    /// exif = EXIF 拍摄时间（含机身校正），mtime = 文件修改时间兜底
    time_source: String,
    time_text: Option<String>,
    width: Option<i64>,
    height: Option<i64>,
    camera_model: Option<String>,
    camera_serial: Option<String>,
    lens: Option<String>,
    focal_len: Option<f64>,
    aperture: Option<f64>,
    shutter: Option<String>,
    iso: Option<i64>,
    orientation: Option<i64>,
    exif_ok: bool,
    indexed_text: Option<String>,
    sharpness: Option<f64>,
    overexposed: Option<f64>,
    underexposed: Option<f64>,
    /// 缩略图链路最终用的解码来源（内嵌预览 / 标准解码 / RAW 解码 / 占位图）
    decode_path: Option<String>,
    fingerprint: String,
    content_hash: Option<String>,
    phash: Option<String>,
    decision: String,
    stars: i64,
    /// 色标：'' / red / yellow / green / blue / purple
    color: String,
    /// 闭眼检测：检出的人脸数，NULL = 还没检测过
    faces: Option<i64>,
    /// 最闭的那只眼睛的 EAR
    eye_ratio: Option<f64>,
    siblings: Vec<SiblingFile>,
}

fn photo_detail_of(conn: &Connection, id: i64) -> anyhow::Result<PhotoDetail> {
    let sql = format!(
        "SELECT p.path, p.pair_key, p.file_kind, p.is_primary, p.file_size,
                p.width, p.height,
                {TIME_TEXT},
                CASE WHEN p.taken_at_corrected IS NOT NULL THEN 'exif' ELSE 'mtime' END,
                p.camera_model, p.camera_serial, p.lens, p.focal_len, p.aperture,
                p.shutter, p.iso, p.orientation, p.exif_ok,
                strftime('%Y-%m-%d %H:%M:%S', p.indexed_at, 'unixepoch', 'localtime'),
                p.sharpness, p.overexposed, p.underexposed, p.decode_path,
                p.fingerprint, p.content_hash, p.phash,
                {DECISION}, {STARS}, {COLOR}, p.faces, p.eye_ratio
         {FROM_PHOTOS} WHERE p.id = ?1"
    );

    let mut detail = conn.query_row(&sql, [id], |r| {
        Ok(PhotoDetail {
            id,
            path: r.get(0)?,
            pair_key: r.get(1)?,
            file_kind: r.get(2)?,
            is_primary: r.get(3)?,
            file_size: r.get(4)?,
            width: r.get(5)?,
            height: r.get(6)?,
            time_text: r.get(7)?,
            time_source: r.get(8)?,
            camera_model: r.get(9)?,
            camera_serial: r.get(10)?,
            lens: r.get(11)?,
            focal_len: r.get(12)?,
            aperture: r.get(13)?,
            shutter: r.get(14)?,
            iso: r.get(15)?,
            orientation: r.get(16)?,
            exif_ok: r.get(17)?,
            indexed_text: r.get(18)?,
            sharpness: r.get(19)?,
            overexposed: r.get(20)?,
            underexposed: r.get(21)?,
            decode_path: r.get(22)?,
            fingerprint: r.get(23)?,
            content_hash: r.get(24)?,
            phash: r.get(25)?,
            decision: r.get(26)?,
            stars: r.get(27)?,
            color: r.get(28)?,
            faces: r.get(29)?,
            eye_ratio: r.get(30)?,
            file_name: String::new(),
            dir: String::new(),
            siblings: Vec::new(),
        })
    })?;

    let p = std::path::Path::new(&detail.path);
    detail.file_name = p
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| detail.path.clone());
    detail.dir = p
        .parent()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    // 同一次快门的另一半（NEF ↔ JPG）。逐个 stat 一下才知道还在不在——
    // 详情面板是单张操作，几十微秒的 stat 换「文件没了」的明确提示，值。
    let pid = id.to_string();
    detail.siblings = conn
        .prepare(
            "SELECT id, path, file_kind, file_size
             FROM photos WHERE pair_key = ?1 AND id != ?2 ORDER BY path",
        )?
        .query_map([&detail.pair_key, pid.as_str()], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(sid, spath, kind, size)| {
            let exists = std::fs::metadata(&spath).is_ok();
            SiblingFile {
                id: sid,
                path: spath,
                file_kind: kind,
                file_size: size,
                exists,
            }
        })
        .collect();

    Ok(detail)
}

/// 大图详情面板的数据源：按 photo id 查整行 + 同组文件。
#[tauri::command]
async fn photo_detail(state: tauri::State<'_, AppState>, id: i64) -> Result<PhotoDetail, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        photo_detail_of(&conn, id).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 详情面板的「完整 EXIF」：只读打开一次原文件，把 EXIF 里能认出来的都摆出来。
///
/// 索引时为了吞吐只存了十来个字段，剩下的留到「用户真的在看这一张」时现读。
/// 前端拿到的是现成的「小节 / 名称 / 值」三元组，按小节顺序渲染即可。
#[tauri::command]
async fn photo_exif(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Vec<exif_detail::ExifItem>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let (path, orientation): (String, Option<i64>) = conn
            .query_row(
                "SELECT path, orientation FROM photos WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let mut items = exif_detail::read_full(Path::new(&path)).map_err(|e| format!("{e:#}"))?;

        // 对焦框藏在 MakerNote 里，顺手也摆进详情：大图上那个黄框就是这个数。
        // 插在「曝光」小节末尾而不是追加到最后，免得同一个 group 被拆成两段。
        if let Some(af) = af::af_area_of(Path::new(&path), orientation.unwrap_or(1)) {
            let value = format!(
                "横向 {:.1}%、纵向 {:.1}%（区域 {:.1}% × {:.1}%）{}",
                af.x * 100.0,
                af.y * 100.0,
                af.w * 100.0,
                af.h * 100.0,
                af.mode
                    .as_deref()
                    .map(|m| format!(" · {m}"))
                    .unwrap_or_default()
            );
            let at = items
                .iter()
                .rposition(|i| i.group == "曝光")
                .map(|i| i + 1)
                .unwrap_or(items.len());
            items.insert(
                at,
                exif_detail::ExifItem {
                    group: "曝光".to_string(),
                    label: "对焦区域".to_string(),
                    value,
                },
            );
        }
        Ok(items)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 大图上要画的那个对焦框。
///
/// 读原文件的 MakerNote，一张一次、只在开大图且开关打开时调。
/// 读不到（不是尼康 / 机型没写 / 模式不支持）返回 null，前端就不画。
#[tauri::command]
async fn photo_af(
    state: tauri::State<'_, AppState>,
    id: i64,
) -> Result<Option<af::AfArea>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let (path, orientation): (String, Option<i64>) = conn
            .query_row(
                "SELECT path, orientation FROM photos WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        // orientation 决定了对焦框要不要跟着图一起转
        Ok(af::af_area_of(Path::new(&path), orientation.unwrap_or(1)))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn facets_of(conn: &Connection, roots: Option<&[String]>) -> anyhow::Result<LibraryFacets> {
    // 和网格用同一个目录范围，否则侧栏数字是整库的、网格是当前文件夹的，两边对不上
    let (scope, sargs) = scope_where(roots);
    let sp = || rusqlite::params_from_iter(sargs.iter());

    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM photos p WHERE p.is_primary = 1{scope}"),
        sp(),
        |r| r.get(0),
    )?;

    // 选片进度：过目了多少、其中留了多少。
    // 打了星也算「看过」——很多人先打星、再决定去留，只数 keep/reject 会低估进度。
    let (processed, kept): (i64, i64) = conn.query_row(
        &format!(
            "SELECT
                COALESCE(SUM(CASE WHEN {DECISION} IN ('keep', 'reject') OR {STARS} > 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN {DECISION} = 'keep' THEN 1 ELSE 0 END), 0)
             {FROM_PHOTOS} WHERE p.is_primary = 1{scope}"
        ),
        sp(),
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    )?;

    let (both, raw_only, jpg_only) = conn.query_row(
        &format!(
            "SELECT
                COALESCE(SUM(CASE WHEN {HAS_RAW} AND {HAS_JPG} THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN {HAS_RAW} AND NOT {HAS_JPG} THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN NOT {HAS_RAW} AND {HAS_JPG} THEN 1 ELSE 0 END), 0)
             FROM photos p WHERE p.is_primary = 1{scope}"
        ),
        sp(),
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;

    let pair_states = vec![
        Facet {
            key: "all".into(),
            label: "全部".into(),
            count: total,
        },
        Facet {
            key: "both".into(),
            label: "NEF + JPG".into(),
            count: both,
        },
        Facet {
            key: "rawOnly".into(),
            label: "仅 NEF".into(),
            count: raw_only,
        },
        Facet {
            key: "jpgOnly".into(),
            label: "仅 JPG".into(),
            count: jpg_only,
        },
        Facet {
            key: "orphan".into(),
            label: "有缺失（孤立）".into(),
            count: raw_only + jpg_only,
        },
    ];

    let mut cameras = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT COALESCE(p.camera_serial, '__none__') AS serial,
                    COALESCE(MAX(p.camera_model), '未知机身') AS model,
                    COUNT(*) AS n
             FROM photos p
             WHERE p.is_primary = 1{scope}
             GROUP BY serial
             ORDER BY n DESC, model"
        ))?;
        let mut rows = stmt.query(sp())?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let model: String = r.get(1)?;
            let label = if key == NONE_KEY {
                format!("{model}（无序列号）")
            } else {
                model
            };
            cameras.push(Facet {
                key,
                label,
                count: r.get(2)?,
            });
        }
    }

    let day_expr = format!("({DAY_KEY})");
    let distinct_days: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM (SELECT DISTINCT {day_expr} AS d FROM photos p
                WHERE p.is_primary = 1 AND {day_expr} IS NOT NULL{scope})"
        ),
        sp(),
        |r| r.get(0),
    )?;
    let undated: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM photos p WHERE p.is_primary = 1 AND {day_expr} IS NULL{scope}"
        ),
        sp(),
        |r| r.get(0),
    )?;

    let mut days = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT {day_expr} AS d, COUNT(*) AS n
             FROM photos p
             WHERE p.is_primary = 1 AND {day_expr} IS NOT NULL{scope}
             GROUP BY d
             ORDER BY d DESC
             LIMIT {DAY_FACET_LIMIT}"
        ))?;
        let mut rows = stmt.query(sp())?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            days.push(Facet {
                // 展示文案（「09-16 周三」）交给前端拼，它更清楚今天是哪一年
                label: key.clone(),
                key,
                count: r.get(1)?,
            });
        }
    }

    if undated > 0 {
        days.push(Facet {
            key: NONE_KEY.into(),
            label: "无时间信息".into(),
            count: undated,
        });
    }

    // 选片状态与星级——选片时最常用的两个维度，所以放在分面里最前面。
    let (dec_none, dec_keep, dec_reject) = conn.query_row(
        &format!(
            "SELECT
                COALESCE(SUM(CASE WHEN {DECISION} = 'none'   THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN {DECISION} = 'keep'   THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN {DECISION} = 'reject' THEN 1 ELSE 0 END), 0)
             {FROM_PHOTOS} WHERE p.is_primary = 1{scope}"
        ),
        sp(),
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        },
    )?;

    let decisions = vec![
        Facet {
            key: "all".into(),
            label: "全部".into(),
            count: total,
        },
        Facet {
            key: "none".into(),
            label: "未标记".into(),
            count: dec_none,
        },
        Facet {
            key: "keep".into(),
            label: "保留".into(),
            count: dec_keep,
        },
        Facet {
            key: "reject".into(),
            label: "淘汰".into(),
            count: dec_reject,
        },
    ];

    // 星级分面固定给出 0–5 六档（缺的补 0），这样侧栏里的位置不会随着
    // 「刚好有几档没照片」而跳动——位置固定，眼睛才不用重新找。
    let mut stars = (0..=5)
        .map(|s| Facet {
            key: s.to_string(),
            label: if s == 0 {
                "未打星".into()
            } else {
                "★".repeat(s)
            },
            count: 0,
        })
        .collect::<Vec<_>>();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT {STARS} AS s, COUNT(*) {FROM_PHOTOS}
             WHERE p.is_primary = 1{scope} GROUP BY s"
        ))?;
        let mut rows = stmt.query(sp())?;
        while let Some(r) = rows.next()? {
            let s: i64 = r.get(0)?;
            if let Some(slot) = stars.get_mut(s.clamp(0, 5) as usize) {
                slot.count = r.get(1)?;
            }
        }
    }

    // 色标分面同样固定给出全部五档：位置记住了眼睛才不用重新找，
    // 理由与星级那六档一样。
    let mut colors = COLOR_KEYS
        .iter()
        .map(|(key, label)| Facet {
            key: (*key).into(),
            label: (*label).into(),
            count: 0,
        })
        .collect::<Vec<_>>();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLOR} AS c, COUNT(*) {FROM_PHOTOS}
             WHERE p.is_primary = 1{scope} GROUP BY c"
        ))?;
        let mut rows = stmt.query(sp())?;
        while let Some(r) = rows.next()? {
            let c: String = r.get(0)?;
            if let Some(slot) = colors.iter_mut().find(|f| f.key == c) {
                slot.count = r.get(1)?;
            }
        }
    }

    // 画面质量：四档一次数完。没分析过的（NULL）不算进来，
    // 否则刚扫完就显示「几千张糊了」——那是还没算，不是糊。
    // 「疑似闭眼」同理：检测默认关着，没跑过就是 0，不是「没人眨眼」。
    let quality = {
        let sql = format!(
            "SELECT
                COALESCE(SUM(CASE WHEN p.sharpness    IS NOT NULL AND p.sharpness    <  {} THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN p.overexposed  IS NOT NULL AND p.overexposed  >= {} THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN p.underexposed IS NOT NULL AND p.underexposed >= {} THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN p.eye_ratio    IS NOT NULL AND p.eye_ratio    <  {} THEN 1 ELSE 0 END), 0)
             {FROM_PHOTOS} WHERE p.is_primary = 1{scope}",
            analyze::BLUR_THRESHOLD,
            analyze::OVEREXPOSED_THRESHOLD,
            analyze::UNDEREXPOSED_THRESHOLD,
            blink::CLOSED_THRESHOLD,
        );
        let (blur, over, under, blink) = conn.query_row(&sql, sp(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        vec![
            Facet {
                key: "blur".into(),
                label: "可能糊了".into(),
                count: blur,
            },
            Facet {
                key: "over".into(),
                label: "高光溢出".into(),
                count: over,
            },
            Facet {
                key: "under".into(),
                label: "暗部死黑".into(),
                count: under,
            },
            Facet {
                key: "blink".into(),
                label: "疑似闭眼".into(),
                count: blink,
            },
        ]
    };

    // 镜头：和机身一样按型号聚合，读不到镜头的归到「未知」
    let lenses = {
        let mut out: Vec<Facet> = Vec::new();
        let mut stmt = conn.prepare(&format!(
            "SELECT COALESCE(p.lens, '{NONE_KEY}') AS l, COUNT(*)
             {FROM_PHOTOS} WHERE p.is_primary = 1{scope}
             GROUP BY l ORDER BY COUNT(*) DESC, l"
        ))?;
        let mut rows = stmt.query(sp())?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let label = if key == NONE_KEY {
                "未知镜头".into()
            } else {
                key.clone()
            };
            out.push(Facet {
                key,
                label,
                count: r.get(1)?,
            });
        }
        out
    };

    // 焦段 / ISO：固定档位全部列出（包括 0 的），位置不随素材跳动
    let focals = bucket_facets(
        conn,
        "p.focal_len",
        &[
            ("wide", 0.0, 24.0),
            ("normal", 24.0, 70.0),
            ("tele", 70.0, 200.0),
            ("super", 200.0, f64::MAX),
        ],
        scope.as_str(),
        &sargs,
        &["24 以下", "24–70", "70–200", "200 以上"],
    )?;
    let isos = bucket_facets(
        conn,
        "p.iso",
        &[
            ("low", 0.0, 400.0),
            ("mid", 400.0, 1600.0),
            ("high", 1600.0, 6400.0),
            ("veryHigh", 6400.0, f64::MAX),
        ],
        scope.as_str(),
        &sargs,
        &["400 以下", "400–1600", "1600–6400", "6400 以上"],
    )?;

    Ok(LibraryFacets {
        total,
        processed,
        kept,
        pair_states,
        cameras,
        days_truncated: distinct_days > DAY_FACET_LIMIT,
        days,
        decisions,
        stars,
        colors,
        quality,
        lenses,
        focals,
        isos,
    })
}

/// 数固定档位里各有多少张。档位写死在调用方，前后端两侧用同一套 key。
fn bucket_facets(
    conn: &Connection,
    column: &str,
    buckets: &[(&str, f64, f64)],
    scope: &str,
    sargs: &[rusqlite::types::Value],
    labels: &[&str],
) -> anyhow::Result<Vec<Facet>> {
    let cases = buckets
        .iter()
        .map(|(_, lo, hi)| {
            format!(
                "COALESCE(SUM(CASE WHEN {column} >= {lo} AND {column} < {hi} THEN 1 ELSE 0 END), 0)"
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {cases} {FROM_PHOTOS} WHERE p.is_primary = 1{scope}");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query(rusqlite::params_from_iter(sargs.iter()))?;
    let mut out: Vec<Facet> = Vec::new();
    if let Some(r) = rows.next()? {
        for (i, (key, _, _)) in buckets.iter().enumerate() {
            out.push(Facet {
                key: (*key).to_string(),
                label: labels[i].to_string(),
                count: r.get(i)?,
            });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 选片
//
// 这是整个应用存在的理由：把一次拍摄看一遍、留一批、淘汰一批。
// 前面那些索引、筛选、缩略图都只是为了让这一节能跑起来。
// ---------------------------------------------------------------------------

/// 一条语句里塞多少个 id。SQLite 的变量上限是 999，这里留足余量，
/// 免得「全选三千张一起标记」这种最正常不过的操作反而报参数过多。
const ID_CHUNK: usize = 400;

/// 只接受三个已知取值；其余情况（含 `None`）表示「这一项不改」。
///
/// 这样 `apply_decision` 才能既当「打标记」用，又当「只改星级」用。
/// 不理解的字符串一律忽略而不是报错：前端传错一个值，不该让整批标记失败。
fn normalize_decision(d: Option<&str>) -> Option<&'static str> {
    match d {
        Some("keep") => Some("keep"),
        Some("reject") => Some("reject"),
        Some("none") => Some("none"),
        _ => None,
    }
}

/// 把一批照片的选片结果写进库，返回受影响的**照片**数。
///
/// `ids` 是 `photos.id`（卡片上的那个 id），但标记落在 `pair_key` 上：
/// 一次快门里的 NEF 和 JPG 是同一张照片，标记必须同时盖住两个文件。
/// 否则「保留了 RAW、JPG 却还是未标记」，按状态筛选时就会自相矛盾。
fn apply_decision_rows(
    conn: &mut Connection,
    ids: &[i64],
    decision: Option<&str>,
    stars: Option<i64>,
    color: Option<&str>,
) -> anyhow::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }

    let decision = normalize_decision(decision);
    let stars = stars.map(|s| s.clamp(0, 5));
    // 一个字段要表达三种意图（设 / 清 / 不动），所以空串是有意义的取值：
    // Some("") = 清掉色标，None = 别动色标。图省事把空串折成 None 就永远清不掉。
    let color = match color {
        Some(c) => normalize_color(Some(c)).or(Some(String::new())),
        None => None,
    };
    let now = now_epoch();

    let tx = conn.transaction()?;

    // 先去重成 pair_key。同一次快门的 NEF 和 JPG 会给出同一个 key，
    // 所以这里天然做到「标记一对、不是标记一个文件」。
    let mut keys: Vec<String> = Vec::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let holders = vec!["?"; chunk.len()].join(",");
        let mut stmt = tx.prepare(&format!(
            "SELECT DISTINCT pair_key FROM photos WHERE id IN ({holders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |r| {
            r.get::<_, String>(0)
        })?;
        for row in rows {
            keys.push(row?);
        }
    }
    keys.sort();
    keys.dedup();

    {
        let mut up = tx.prepare(
            "INSERT INTO decisions(pair_key, decision, stars, color, updated_at)
             VALUES(?1, COALESCE(?2, 'none'), COALESCE(?3, 0), COALESCE(?4, ''), ?5)
             ON CONFLICT(pair_key) DO UPDATE SET
               decision   = COALESCE(?2, decisions.decision),
               stars      = COALESCE(?3, decisions.stars),
               color      = COALESCE(?4, decisions.color),
               updated_at = ?5",
        )?;
        for key in &keys {
            up.execute(rusqlite::params![key, decision, stars, color, now])?;
        }
    }

    tx.commit()?;
    Ok(keys.len())
}

/// 对一批照片应用选片结果。前端把「选中的卡片」原样发过来。
#[tauri::command]
async fn apply_decision(
    state: tauri::State<'_, AppState>,
    ids: Vec<i64>,
    decision: Option<String>,
    stars: Option<i64>,
    color: Option<String>,
) -> Result<usize, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<usize, String> {
        let mut conn = db.lock().map_err(|e| e.to_string())?;
        apply_decision_rows(
            &mut conn,
            &ids,
            decision.as_deref(),
            stars,
            color.as_deref(),
        )
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 当前筛选条件下**全部**照片的 id，不只是已经滚出来的那几页。
///
/// 「全选」必须作用在完整的筛选结果上。如果只选中已加载的 120 张，
/// 用户会以为整批都标记好了——这是一种安静地做错事，比报错糟糕得多。
#[tauri::command]
async fn list_pair_ids(
    state: tauri::State<'_, AppState>,
    filter: Option<PairFilter>,
) -> Result<Vec<i64>, String> {
    let db = state.db.clone();
    let filter = filter.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<i64>, String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let (where_sql, args) = build_where(&filter);
        let mut stmt = conn
            .prepare(&format!(
                "SELECT p.id {FROM_PHOTOS} WHERE {where_sql} {}",
                order_by(filter.sort.as_deref())
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                r.get::<_, i64>(0)
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// 导出
//
// 原则：「原片只读」。导出一律是**复制**，不删、不移、不改名。
// 淘汰的照片也只是在清单里记一笔，真正的删除永远留给用户自己动手——
// 那张照片只被拍过一次，程序没有资格替他做这个决定。
// ---------------------------------------------------------------------------

/// 一个待导出的原文件。
#[derive(Clone)]
struct ExportFile {
    path: String,
    size: i64,
}

/// 一条一次快门的导出记录：元数据 + 它涉及的所有原文件。
struct ExportPair {
    pair_key: String,
    decision: String,
    stars: i64,
    taken_at_text: Option<String>,
    camera_model: Option<String>,
    lens: Option<String>,
    focal_len: Option<f64>,
    aperture: Option<f64>,
    shutter: Option<String>,
    iso: Option<i64>,
    files: Vec<ExportFile>,
}

/// 导出时要哪些文件。
///
/// 一次快门通常留下 RAW + JPG 两份，但很多时候只需要其中一种：
/// 交给后期只想要 RAW，发预览只想要 JPG。默认 `both` 保持原来的行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileScope {
    Both,
    Raw,
    Jpeg,
}

impl FileScope {
    fn parse(s: &str) -> Self {
        match s {
            "raw" => Self::Raw,
            "jpeg" => Self::Jpeg,
            _ => Self::Both,
        }
    }
    fn keeps(self, kind: &str) -> bool {
        match self {
            Self::Both => true,
            Self::Raw => kind == "raw",
            Self::Jpeg => kind == "jpeg",
        }
    }
}

/// 重命名模板的默认形式：保持原文件名。
const DEFAULT_NAME_TEMPLATE: &str = "{name}";

/// 模板里能用的变量，按顺序填进 `render_name`。
struct NameCtx<'a> {
    /// 原文件名（不含扩展名）
    stem: &'a str,
    /// 扩展名（不含点）
    ext: &'a str,
    /// 拍摄日期 YYYYMMDD
    date: &'a str,
    /// 拍摄时间 HHMMSS
    time: &'a str,
    /// 这一批里的序号，从 1 开始
    seq: usize,
    stars: i64,
    camera: &'a str,
    pair: &'a str,
}

/// 把重命名模板渲染成文件名。
///
/// 支持 `{name}` `{ext}` `{date}` `{time}` `{seq}` `{stars}` `{camera}` `{pair}`。
/// 模板是纯字符串处理，所以能直接单测——「用户写了个没见过的变量」这种
/// 情况必须一眼看出结果，而不是等导出一千张之后才发现名字全乱了。
fn render_name(template: &str, ctx: &NameCtx) -> String {
    let values: &[(&str, String)] = &[
        ("{name}", ctx.stem.to_string()),
        ("{ext}", ctx.ext.to_string()),
        ("{date}", ctx.date.to_string()),
        ("{time}", ctx.time.to_string()),
        ("{seq}", format!("{:04}", ctx.seq)),
        ("{stars}", ctx.stars.to_string()),
        ("{camera}", ctx.camera.to_string()),
        ("{pair}", ctx.pair.to_string()),
    ];

    let mut out = template.to_string();
    for (k, v) in values {
        out = out.replace(k, v);
    }

    // 模板里没写 {ext} 就补上，避免导出一堆没有扩展名的文件
    if !out
        .to_lowercase()
        .ends_with(&format!(".{}", ctx.ext.to_lowercase()))
    {
        out.push('.');
        out.push_str(ctx.ext);
    }

    // 文件名里的非法字符统一换成下划线（Windows 最严格，两边一起遵守）
    const BAD: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];
    let cleaned: String = out
        .chars()
        .map(|c| if BAD.contains(&c) { '_' } else { c })
        .collect();

    if cleaned.trim().is_empty() || cleaned == format!(".{}", ctx.ext) {
        format!("{}.{}", ctx.stem, ctx.ext)
    } else {
        cleaned
    }
}

/// 拆出主文件名与扩展名。
fn split_ext(path: &str) -> (String, String) {
    let p = std::path::Path::new(path);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    (stem, ext)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportSummary {
    dest: String,
    manifest: String,
    /// 参与导出的照片数（一次快门算一张）
    photos: usize,
    /// 涉及的原文件数（NEF + JPG 分开算）
    files: usize,
    copied: usize,
    /// 目标文件夹里已有同名同大小的文件，跳过。重复导出不会滚出一堆副本。
    skipped: usize,
    failed: usize,
    bytes: u64,
    elapsed_ms: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportProgress {
    /// listing / copying / manifest
    phase: &'static str,
    done: usize,
    total: usize,
}

/// 按筛选条件把照片导出到目标文件夹。
///
/// `mode`：
/// - `copy`：把配对涉及的**全部原文件**复制过去（NEF 和 JPG 都算），再写一份 CSV 清单
/// - `list`：只写 CSV 清单，不碰任何文件
///
/// 两种模式都写清单。清单才是这个功能的底线——即使一个文件都没复制，
/// 它也把「留下了哪些、淘汰了哪些」变成了一份可以交给别人、可以留档的东西。
#[tauri::command]
async fn export_selection(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    filter: Option<PairFilter>,
    dest: String,
    mode: String,
    // 要哪些文件：raw / jpeg / both（默认）
    scope: Option<String>,
    // 重命名模板，默认 {name}（保持原文件名）
    template: Option<String>,
) -> Result<ExportSummary, String> {
    let db = state.db.clone();
    let filter = filter.unwrap_or_default();
    let scope = FileScope::parse(scope.as_deref().unwrap_or("both"));
    let template = template
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_NAME_TEMPLATE)
        .to_string();
    tauri::async_runtime::spawn_blocking(move || -> Result<ExportSummary, String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let copy_files = mode == "copy";
        export_rows(
            &conn,
            &filter,
            Path::new(&dest),
            copy_files,
            scope,
            &template,
            // 推送失败不是错误：窗口关了而已，复制该继续跑完
            |p| {
                let _ = app.emit("export://progress", &p);
            },
        )
        .map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 导出主体。进度用回调上报而不是直接拿 AppHandle——
/// 这样这个函数不依赖 Tauri 运行时，测试里可以直接调。
fn export_rows<F>(
    conn: &Connection,
    filter: &PairFilter,
    dest: &Path,
    copy_files: bool,
    scope: FileScope,
    template: &str,
    on_progress: F,
) -> anyhow::Result<ExportSummary>
where
    F: Fn(ExportProgress),
{
    use anyhow::Context;

    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(dest)
        .with_context(|| format!("无法创建目标文件夹：{}", dest.display()))?;

    on_progress(ExportProgress {
        phase: "listing",
        done: 0,
        total: 0,
    });

    let pairs = collect_export_pairs(conn, filter, scope)?;

    let files_total: usize = pairs.iter().map(|p| p.files.len()).sum();
    let mut copied = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut bytes = 0u64;

    if copy_files {
        let mut done = 0usize;
        for (idx, pair) in pairs.iter().enumerate() {
            let (date, time) = split_datetime(pair.taken_at_text.as_deref());
            for f in &pair.files {
                done += 1;
                let src_path = Path::new(&f.path);
                let (stem, ext) = split_ext(&f.path);
                let name = render_name(
                    template,
                    &NameCtx {
                        stem: &stem,
                        ext: &ext,
                        date: &date,
                        time: &time,
                        seq: idx + 1,
                        stars: pair.stars,
                        camera: pair.camera_model.as_deref().unwrap_or(""),
                        pair: &pair.pair_key,
                    },
                );

                match plan_target(dest, &name, f.size as u64) {
                    // 已经复制过同一份（同名同大小），这一遍就跳过
                    None => skipped += 1,
                    Some(target) => match std::fs::copy(src_path, &target) {
                        Ok(n) => {
                            copied += 1;
                            bytes += n;
                        }
                        Err(_) => failed += 1,
                    },
                }

                if done % 32 == 0 || done == files_total {
                    on_progress(ExportProgress {
                        phase: "copying",
                        done,
                        total: files_total,
                    });
                }
            }
        }
    }

    on_progress(ExportProgress {
        phase: "manifest",
        done: 0,
        total: pairs.len(),
    });

    let manifest_path = dest.join("选片清单.csv");
    std::fs::write(&manifest_path, manifest_csv(&pairs))
        .with_context(|| format!("无法写入清单：{}", manifest_path.display()))?;

    Ok(ExportSummary {
        dest: dest.to_string_lossy().to_string(),
        manifest: manifest_path.to_string_lossy().to_string(),
        photos: pairs.len(),
        files: files_total,
        copied,
        skipped,
        failed,
        bytes,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

/// 取出筛选命中的照片，以及每张照片涉及的所有原文件。
fn collect_export_pairs(
    conn: &Connection,
    filter: &PairFilter,
    scope: FileScope,
) -> anyhow::Result<Vec<ExportPair>> {
    let (where_sql, args) = build_where(filter);

    let mut pairs: Vec<ExportPair> = Vec::new();
    {
        let sql = format!(
            "SELECT p.pair_key, {DECISION}, {STARS}, {TIME_TEXT},
                    p.camera_model, p.lens, p.focal_len, p.aperture, p.shutter, p.iso
             {FROM_PHOTOS}
             WHERE {where_sql}
             {}",
            order_by(filter.sort.as_deref())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(ExportPair {
                pair_key: r.get(0)?,
                decision: r.get(1)?,
                stars: r.get(2)?,
                taken_at_text: r.get(3)?,
                camera_model: r.get(4)?,
                lens: r.get(5)?,
                focal_len: r.get(6)?,
                aperture: r.get(7)?,
                shutter: r.get(8)?,
                iso: r.get(9)?,
                files: Vec::new(),
            })
        })?;
        for row in rows {
            pairs.push(row?);
        }
    }

    let mut by_key: HashMap<String, Vec<ExportFile>> = HashMap::new();
    for chunk in pairs.chunks(ID_CHUNK) {
        let holders = vec!["?"; chunk.len()].join(",");
        let keys: Vec<&str> = chunk.iter().map(|p| p.pair_key.as_str()).collect();
        let mut stmt = conn.prepare(&format!(
            "SELECT pair_key, path, file_size, file_kind FROM photos WHERE pair_key IN ({holders})"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(keys.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (k, path, size, kind) = row?;
            // 「仅 RAW / 仅 JPG」在这一步就筛掉，后面的计数和清单都只算选中的
            if !scope.keeps(&kind) {
                continue;
            }
            by_key.entry(k).or_default().push(ExportFile { path, size });
        }
    }

    for p in &mut pairs {
        if let Some(files) = by_key.remove(&p.pair_key) {
            // 排序让清单和复制顺序稳定：同样的输入跑两次，结果一样
            let mut files = files;
            files.sort_by(|a, b| a.path.cmp(&b.path));
            p.files = files;
        }
    }

    Ok(pairs)
}

/// 从「2026-09-16 10:20:37」里拆出 `20260916` 和 `102037`。
/// 拿不到就返回空串——模板里对应的位置会空着，但不至于崩。
fn split_datetime(text: Option<&str>) -> (String, String) {
    let s = text.unwrap_or("").trim();
    if s.len() < 19 {
        return (String::new(), String::new());
    }
    let date: String = s[..10].chars().filter(|c| c.is_ascii_digit()).collect();
    let time: String = s[11..19].chars().filter(|c| c.is_ascii_digit()).collect();
    (date, time)
}

/// 给一个源文件挑目标路径。
///
/// 返回 `None` 表示「目标里已经有同名同大小的文件了」——直接跳过。
/// 这是**可重复运行**的关键：同一批照片导出两遍，第二遍不会滚出
/// 一堆 `DSC_0001-1.NEF`、`DSC_0001-2.NEF`。
///
/// 同名但大小不同（真的撞名了）才退让到 `-1`、`-2` 后缀。
fn plan_target(dir: &Path, src_name: &str, size: u64) -> Option<PathBuf> {
    let same_size = |p: &Path| {
        std::fs::metadata(p)
            .map(|m| m.len() == size)
            .unwrap_or(false)
    };

    let first = dir.join(src_name);
    if !first.exists() {
        return Some(first);
    }
    if same_size(&first) {
        return None;
    }

    let (stem, ext) = match src_name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (src_name.to_string(), None),
    };

    for n in 1..1000 {
        let cand = dir.join(match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        });
        if !cand.exists() {
            return Some(cand);
        }
        if same_size(&cand) {
            return None;
        }
    }
    None
}

/// CSV 里一个字段的写法。含逗号、引号、换行的值必须整体加引号，
/// 内部引号翻倍——照片文件名里出现逗号是常事，不转义会把表格撑歪。
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 拼一行。字段个数由调用处保证——11 列的表头和数据行必须一一对应，
/// 用 `format!` 手工数逗号正是最容易数错的地方。
fn csv_row(fields: &[String]) -> String {
    let mut line = fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
    line.push('\n');
    line
}

fn manifest_csv(pairs: &[ExportPair]) -> String {
    let mut out = String::from("\u{feff}"); // BOM：Windows 版 Excel 才认得 UTF-8
    out.push_str("决定,星级,文件名,拍摄时间,机身,镜头,焦距mm,光圈,快门,ISO,完整路径\n");

    let blank = String::new;
    for p in pairs {
        let decision = match p.decision.as_str() {
            "keep" => "保留",
            "reject" => "淘汰",
            _ => "未标记",
        };

        if p.files.is_empty() {
            // 索引里有、磁盘上却找不到的，也要出现在清单里——悄悄漏掉它
            // 才是真的坑：用户会以为这张已经处理过了。
            out.push_str(&csv_row(&[
                decision.to_string(),
                p.stars.to_string(),
                "(文件缺失)".to_string(),
                p.taken_at_text.clone().unwrap_or_default(),
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
                blank(),
                p.pair_key.clone(),
            ]));
            continue;
        }

        for f in &p.files {
            let name = Path::new(&f.path)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            out.push_str(&csv_row(&[
                decision.to_string(),
                p.stars.to_string(),
                name,
                p.taken_at_text.clone().unwrap_or_default(),
                p.camera_model.clone().unwrap_or_default(),
                p.lens.clone().unwrap_or_default(),
                p.focal_len.map(|v| format!("{v:.1}")).unwrap_or_default(),
                p.aperture.map(|v| format!("{v:.1}")).unwrap_or_default(),
                p.shutter.clone().unwrap_or_default(),
                p.iso.map(|v| v.to_string()).unwrap_or_default(),
                f.path.clone(),
            ]));
        }
    }

    out
}

/// 一张缩略图，直接以 data URL 交给前端。
///
/// 用 data URL 而不是本地文件协议：省掉一套协议作用域配置，Mac / Windows 行为一致。
/// 前端只在卡片进入视口时才请求，所以一次 IPC 的量是几十张而不是几千张。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ThumbPayload {
    id: i64,
    size: u32,
    data_url: String,
    /// embedded-jpeg（抠内嵌预览）/ file-jpeg（文件本身就是 JPEG）/ cached
    route: String,
    source_width: u32,
    source_height: u32,
    from_cache: bool,
}

/// 一次请求要生成哪几档缩略图。
///
/// 单独抽出来是为了能测：档位分派写错的话，放大时会拿到 512px 的图还浑然不觉。
/// 网格一次要两档（顺带把 micro 建了，同一次解码），放大查看各档单独要。
fn sizes_for(size: u32) -> Vec<u32> {
    match size {
        thumb::SIZE_PREVIEW => vec![thumb::SIZE_PREVIEW],
        thumb::SIZE_LOUPE => vec![thumb::SIZE_LOUPE],
        _ => vec![thumb::SIZE_GRID, thumb::SIZE_MICRO],
    }
}

/// 取（必要时生成）指定照片的缩略图。
///
/// 这是「NEF 为什么看不了」的答案：NEF 是 RAW 容器，WebView 天生解不了，
/// 必须由 Rust 侧把内嵌的 JPEG 预览抠出来再喂给它。
#[tauri::command]
async fn photo_thumbnail(
    state: tauri::State<'_, AppState>,
    id: i64,
    size: u32,
) -> Result<ThumbPayload, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<ThumbPayload, String> {
        // 锁只用来取元数据，解码和编码都在锁外做，否则会串行化掉所有缩略图请求
        let (path, fingerprint, orientation) = {
            let conn = db.lock().map_err(|e| e.to_string())?;
            conn.query_row(
                "SELECT path, fingerprint, orientation FROM photos WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .map_err(|e| format!("读取照片记录失败（id={id}）：{e}"))?
        };

        let sizes = sizes_for(size);
        let wanted = sizes[0];

        // 解码一张 NEF 的预览要上百 MB 内存，这里排队而不是任由并发
        let _permit = thumb::global_limiter().acquire();

        let root = paths::thumbs_dir().map_err(|e| format!("{e:#}"))?;
        let (infos, _) = thumb::ensure(&root, &fingerprint, Path::new(&path), &sizes, orientation)
            .map_err(|e| format!("{e:#}"))?;

        // 刚写过新文件，顺手看看缓存是不是该瘦身了（内部限流，不会每次都扫）
        thumb::enforce_if_needed(&root);

        let info = &infos[0];
        debug_assert_eq!(infos.len(), sizes.len());

        let bytes = std::fs::read(&info.path)
            .map_err(|e| format!("缩略图读取失败（{}）：{e}", info.path.display()))?;
        let data_url = format!(
            "data:image/jpeg;base64,{}",
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes)
        );

        // 记下实际走的解码路径，出问题时一眼能看出是哪一级生效了
        if !info.from_cache {
            if let Ok(conn) = db.lock() {
                let _ = conn.execute(
                    "UPDATE photos SET decode_path = ?1 WHERE id = ?2",
                    rusqlite::params![info.route, id],
                );
            }
        }

        Ok(ThumbPayload {
            id,
            size: wanted,
            data_url,
            route: info.route.clone(),
            source_width: info.source_width,
            source_height: info.source_height,
            from_cache: info.from_cache,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 缓存占用一览。
///
/// 缩略图缓存是 S·P 里唯一会自己不断长大的东西：看过的每张照片都留 2~3 份 JPEG，
/// 一次拍摄几千张就是几百 MB。而它**随时可以重建**（原片没动，重新抠一次预览即可），
/// 所以该清就清，不必心疼。
///
/// 三个数字一起给，是因为「清缓存」在别的应用里往往等于「把我的活儿清没了」——
/// 这里必须让用户一眼看出：哪些能放心清，哪些清了会丢东西。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CacheStats {
    thumbs_files: u64,
    thumbs_bytes: u64,
    thumbs_dir: String,
    /// 主库 + WAL + SHM。只算主库会出现「明明清了还是这么大」的假象。
    db_bytes: u64,
    db_path: String,
    photos: i64,
    /// 真正有内容的选片结果（打过分或标过保留/淘汰的 pair 数）。
    decisions: i64,
    /// 缩略图缓存的容量上限。超过会自动淘汰最久没用的，前端据此显示「已用 / 上限」。
    thumbs_limit_bytes: u64,
}

/// 目录占用（文件数 + 字节数）。目录不存在算 0，不报错。
fn dir_usage(root: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in walkdir::WalkDir::new(root).into_iter().flatten() {
        if let Ok(md) = entry.metadata() {
            if md.is_file() {
                files += 1;
                bytes += md.len();
            }
        }
    }
    (files, bytes)
}

fn db_usage(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let extra = PathBuf::from(format!("{}{suffix}", path.display()));
        total += std::fs::metadata(&extra).map(|m| m.len()).unwrap_or(0);
    }
    total
}

fn cache_stats_of(conn: &Connection) -> anyhow::Result<CacheStats> {
    let thumbs_dir = paths::thumbs_dir()?;
    let (thumbs_files, thumbs_bytes) = dir_usage(&thumbs_dir);
    let db_path = paths::db_path()?;

    Ok(CacheStats {
        thumbs_files,
        thumbs_bytes,
        thumbs_dir: thumbs_dir.to_string_lossy().to_string(),
        db_bytes: db_usage(&db_path),
        db_path: db_path.to_string_lossy().to_string(),
        photos: conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))?,
        decisions: conn.query_row(
            "SELECT COUNT(*) FROM decisions WHERE decision <> 'none' OR stars > 0",
            [],
            |r| r.get(0),
        )?,
        thumbs_limit_bytes: thumb::DEFAULT_MAX_CACHE_BYTES,
    })
}

#[tauri::command]
async fn cache_stats(state: tauri::State<'_, AppState>) -> Result<CacheStats, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        cache_stats_of(&conn).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 清缓存，三档：
///
/// - `thumbs`：只删缩略图文件。索引和选片标记都在，下次浏览会自动重建。
/// - `index`：索引 + 缩略图一起清。**选片标记保留**——它挂在 pair_key 上，
///   重新扫描后会自动回到照片上（这是 `decisions` 表的设计前提）。
/// - `all`：连选片标记一起删。不可恢复，前端要再确认一次。
///
/// 无论哪一档都不碰原片：这里删的全是 S·P 自己在数据目录里生成的文件。
#[tauri::command]
async fn clear_cache(
    state: tauri::State<'_, AppState>,
    scope: String,
) -> Result<CacheStats, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<CacheStats, String> {
        // 1) 缩略图：删内容、留目录，省得下次还要重建父目录
        let root = paths::thumbs_dir().map_err(|e| format!("{e:#}"))?;
        purge_dir_contents(&root).map_err(|e| format!("{e:#}"))?;

        let mut conn = db.lock().map_err(|e| e.to_string())?;
        purge_index(&mut conn, &scope).map_err(|e| format!("{e:#}"))?;

        cache_stats_of(&conn).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 清索引（`index`）或连选片标记一起清（`all`）。`thumbs` 到这里什么都不做。
fn purge_index(conn: &mut Connection, scope: &str) -> anyhow::Result<()> {
    if scope != "index" && scope != "all" {
        return Ok(());
    }
    // 用事务：索引和标记要么一起清干净，要么一个都不动
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM photos", [])?;
    if scope == "all" {
        tx.execute("DELETE FROM decisions", [])?;
    }
    tx.commit()?;
    // DELETE 只是把页面标成空闲，文件大小不变——必须 VACUUM 才真的还空间
    conn.execute_batch("VACUUM")?;
    Ok(())
}

/// 清空目录下的所有条目，但保留目录本身。
///
/// 逐个删而不是 `remove_dir_all` 再重建：后者在目录被别处占用（比如防病毒软件
/// 正在扫）时会整个失败，而逐个删最多留下几个删不掉的残留，下次还能重试。
fn purge_dir_contents(root: &Path) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let entries =
        std::fs::read_dir(root).map_err(|e| anyhow::anyhow!("无法读取 {}: {e}", root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let res = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        // 单个文件删不掉（被占用、权限）不该让整次清理失败
        if let Err(e) = res {
            eprintln!("跳过无法删除的缓存文件 {}: {e}", path.display());
        }
    }
    Ok(())
}

/// 设置某台机身的时间偏移（多机身时钟不同步的校正，详见方案第七节）。
#[tauri::command]
async fn set_camera_offset(
    state: tauri::State<'_, AppState>,
    serial: String,
    offset_seconds: i64,
) -> Result<(), String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO camera_bodies(serial, time_offset_seconds, first_seen_at)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(serial) DO UPDATE SET time_offset_seconds = excluded.time_offset_seconds",
            rusqlite::params![serial, offset_seconds, now_epoch()],
        )
        .map_err(|e| e.to_string())?;
        // 已有照片的校正时间要跟着重算
        conn.execute(
            "UPDATE photos
                SET taken_at_corrected = taken_at + COALESCE(
                    (SELECT time_offset_seconds FROM camera_bodies cb
                      WHERE cb.serial = photos.camera_serial), 0)
              WHERE taken_at IS NOT NULL",
            [],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// 列出库里出现过的机身及其时间偏移，供「时间校正」界面用。
///
/// 单机身时这一栏没什么用，但两台机身时钟不同步时，排序和连拍分组会全乱——
/// 那时候它是唯一的解药。
#[tauri::command]
async fn list_camera_bodies(
    state: tauri::State<'_, AppState>,
    roots: Option<Vec<String>>,
) -> Result<Vec<CameraBody>, String> {
    let db = state.db.clone();
    let roots = roots.unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let (scope, sargs) = scope_where(if roots.is_empty() { None } else { Some(&roots) });
        let mut stmt = conn
            .prepare(&format!(
                "SELECT COALESCE(p.camera_serial, '{NONE_KEY}') AS serial,
                        COALESCE(MAX(p.camera_model), '未知机身') AS model,
                        COALESCE((SELECT cb.time_offset_seconds FROM camera_bodies cb
                                   WHERE cb.serial = p.camera_serial), 0) AS off,
                        COUNT(*) AS n
                 {FROM_PHOTOS} WHERE p.is_primary = 1{scope}
                 GROUP BY serial ORDER BY n DESC, model"
            ))
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(sargs.iter()), |r| {
                Ok(CameraBody {
                    serial: r.get(0)?,
                    model: r.get(1)?,
                    offset_seconds: r.get(2)?,
                    photos: r.get(3)?,
                })
            })
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| e.to_string())?);
        }
        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct CameraBody {
    serial: String,
    model: String,
    /// 正数＝这台机身的时钟比实际快（要把时间往回拨）
    offset_seconds: i64,
    photos: i64,
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 连拍分组
//
// 连着按快门会留下一串几乎一样的照片，真正要做的决定只有「留哪一张」。
// 一张张翻过去对比太慢，所以先按时间把它们聚成组，一次看完整串再挑。
// ---------------------------------------------------------------------------

/// 一组连拍里的一张。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SimilarMember {
    id: i64,
    pair_key: String,
    name: String,
    time_text: Option<String>,
    decision: String,
    stars: i64,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SimilarGroup {
    /// 组里第一张的 pair_key —— 界面上靠它稳定标识这一组
    key: String,
    size: usize,
    /// 组内时间跨度（秒）
    span_secs: i64,
    start_text: Option<String>,
    members: Vec<SimilarMember>,
}

/// 相邻两张间隔不超过 gap（秒）就归为同一组。
///
/// 输入必须已按时间升序。抽成纯函数是为了能单测——
/// 「隔了 2.9 秒算不算同一组」这种边界，肉眼看界面是说不清的。
fn cluster_by_gap(times: &[i64], gap: i64) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();

    for (i, &t) in times.iter().enumerate() {
        match current.last() {
            Some(&last) if t - times[last] <= gap => current.push(i),
            Some(_) => {
                groups.push(std::mem::take(&mut current));
                current.push(i);
            }
            None => current.push(i),
        }
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

/// 默认时间间隔：3 秒。
///
/// 高速连拍间隔不到 1 秒，慢慢拍同一个场景也就几秒一张。定得太小，
/// 会把「同一个姿势连拍三张」切成三组，那就没意义了。
const DEFAULT_GROUP_GAP_SECS: i64 = 3;

/// 一次最多返回多少组。连拍多的时候组数能到几百，全塞给前端没意义。
const MAX_GROUPS: usize = 300;

/// 找出筛选范围内的连拍分组（按拍摄时间聚类）。
#[tauri::command]
async fn similar_groups(
    state: tauri::State<'_, AppState>,
    filter: Option<PairFilter>,
    gap_secs: Option<i64>,
) -> Result<Vec<SimilarGroup>, String> {
    let db = state.db.clone();
    let filter = filter.unwrap_or_default();
    let gap = gap_secs.unwrap_or(DEFAULT_GROUP_GAP_SECS).max(0);

    tauri::async_runtime::spawn_blocking(move || -> Result<Vec<SimilarGroup>, String> {
        let conn = db.lock().map_err(|e| e.to_string())?;
        let (where_sql, args) = build_where(&filter);

        // 按 pair_key 聚合：一次快门算一张，NEF 和 JPG 不重复计数。
        // 读不到拍摄时间的（EXIF 缺失）不参与——没有时间就谈不上「连着拍」。
        let sql = format!(
            "SELECT MIN(p.id), p.pair_key,
                    MIN(COALESCE(p.taken_at_corrected, p.mtime)),
                    MIN({TIME_TEXT}),
                    {DECISION}, {STARS}, MIN(p.path)
             {FROM_PHOTOS}
             WHERE {where_sql} AND COALESCE(p.taken_at_corrected, p.mtime) IS NOT NULL
             GROUP BY p.pair_key
             ORDER BY 3"
        );

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        let times: Vec<i64> = rows.iter().map(|r| r.2).collect();
        let mut out: Vec<SimilarGroup> = Vec::new();

        for cluster in cluster_by_gap(&times, gap) {
            if cluster.len() < 2 {
                continue;
            }
            let members: Vec<SimilarMember> = cluster
                .iter()
                .map(|&i| {
                    let (id, pair_key, _, time_text, decision, stars, path) = &rows[i];
                    SimilarMember {
                        id: *id,
                        pair_key: pair_key.clone(),
                        name: std::path::Path::new(path)
                            .file_name()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        time_text: time_text.clone(),
                        decision: decision.clone(),
                        stars: *stars,
                    }
                })
                .collect();

            out.push(SimilarGroup {
                key: rows[cluster[0]].1.clone(),
                size: members.len(),
                span_secs: times[*cluster.last().unwrap()] - times[cluster[0]],
                start_text: rows[cluster[0]].3.clone(),
                members,
            });

            if out.len() >= MAX_GROUPS {
                break;
            }
        }

        Ok(out)
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// 关于窗口
//
// 系统原生的「关于」面板只认图标 + 名称 + 版本号，塞不进 slogan，
// 也没法用我们自己的品牌排版。所以 macOS 菜单里的「关于 S·P」
// 打开的是自绘窗口（about.html），原生面板从此不再出现。
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn open_about_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("about") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    if let Err(e) =
        tauri::WebviewWindowBuilder::new(app, "about", tauri::WebviewUrl::App("about.html".into()))
            .title("关于 S·P")
            .inner_size(340.0, 400.0)
            .resizable(false)
            .maximizable(false)
            .minimizable(false)
            .center()
            .build()
    {
        eprintln!("打开关于窗口失败：{e}");
    }
}

/// macOS 应用菜单。
///
/// 沿用默认菜单的骨架，只把「关于」换成自绘窗口。没有这一步，
/// 菜单第一项会走系统原生面板，我们的关于窗口就永远没人打开。
#[cfg(target_os = "macos")]
fn macos_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let about = MenuItemBuilder::with_id("about-sp", "关于 S·P").build(app)?;
    let app_menu = SubmenuBuilder::new(app, "S·P")
        .item(&about)
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, "编辑")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    let window_menu = SubmenuBuilder::new(app, "窗口")
        .minimize()
        .separator()
        .close_window()
        .build()?;

    MenuBuilder::new(app)
        .items(&[&app_menu, &edit_menu, &window_menu])
        .build()
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let db_file = match paths::db_path() {
        Ok(p) => p,
        Err(e) => fatal(&format!("无法定位数据目录：{e:#}")),
    };

    // 数据库打不开时**不再直接退出**。
    // 双击启动看不到终端输出，退出等于「点了没反应」——用户连发生了什么都不知道，
    // 更别说自己怎么修。改成用内存库把应用撑起来，界面给一条明确的红字说明。
    let (conn, startup_error) = match db::open(&db_file) {
        Ok(conn) => {
            // 上次启动失败留下的日志已经过期，清掉——免得下次排查时看错现场
            clear_startup_log(&db_file);
            (conn, None)
        }
        Err(e) => {
            let msg = format!(
                "数据库打不开，本次运行不会保存任何索引。\n路径：{}\n原因：{e:#}",
                db_file.display()
            );
            eprintln!("{msg}");
            write_startup_log(&db_file, &msg);
            match db::open_in_memory() {
                Ok(conn) => (conn, Some(msg)),
                Err(mem_err) => fatal(&format!("{msg}\n\n连内存库也建不起来：{mem_err:#}")),
            }
        }
    };

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // 窗口大小与位置记下来，下次打开回到上次的样子。
        // 默认保存尺寸 / 位置 / 最大化，状态文件落在应用数据目录里。
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // 只允许一个实例。开第二个会把已经开着的那份提到前面——
        // 两个进程各持一个 SQLite 连接写同一个库，选片状态会打架，
        // 而这种问题排查起来极费劲，不如在入口就堵死。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .manage(AppState {
            db: Arc::new(Mutex::new(conn)),
            startup_error,
        })
        // 闭眼检测的模型在哪里，交给打包后的资源目录说了算。
        // 开发态这个目录里没有模型，`blink.rs` 会自己退回仓库里的那一份。
        .setup(|app| {
            if let Ok(dir) = app.path().resource_dir() {
                blink::set_model_dir(dir);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            startup_status,
            scan_folder,
            list_subdirs,
            analyze_library,
            list_camera_bodies,
            library_stats,
            library_facets,
            list_pairs,
            list_pair_ids,
            photo_exif,
            photo_af,
            photo_blink,
            blink_enabled,
            set_blink_enabled,
            analyze_blink,
            cancel_blink,
            photo_detail,
            apply_decision,
            similar_groups,
            export_selection,
            photo_thumbnail,
            cache_stats,
            clear_cache,
            set_camera_offset
        ]);

    // 「关于」菜单只在 macOS 上接管：Windows 的窗口菜单栏放这种品牌页反而碍事，
    // Windows 的版本信息走「设置」类入口的常规做法（以后需要再说）。
    #[cfg(target_os = "macos")]
    let builder = builder.menu(macos_menu).on_menu_event(|app, event| {
        if event.id() == "about-sp" {
            open_about_window(app);
        }
    });

    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 把启动阶段的错误写进数据目录里的 `startup-error.log`。
///
/// 双击 .app / .command 启动时看不到终端输出，这个文件是唯一的线索。
fn write_startup_log(near: &Path, message: &str) {
    let dir = near
        .parent()
        .map(|p| p.to_path_buf())
        .or_else(|| paths::data_dir().ok())
        .unwrap_or_else(std::env::temp_dir);
    let file = dir.join("startup-error.log");
    let _ = std::fs::write(&file, format!("时间戳：{}\n\n{message}\n", now_epoch()));
    eprintln!("（详情已写入 {}）", file.display());
}

/// 启动成功时清掉上一次失败留下的日志。
fn clear_startup_log(near: &Path) {
    if let Some(dir) = near.parent() {
        let file = dir.join("startup-error.log");
        if file.exists() {
            let _ = std::fs::remove_file(file);
        }
    }
}

/// 启动阶段的致命错误：打印到终端，同时落一份日志文件。
///
/// 双击 .app / .command 启动时看不到终端输出，日志文件是唯一的线索，
/// 所以这里不能只 eprintln 就退出。
fn fatal(message: &str) -> ! {
    let text = format!("S·P 启动失败\n\n{message}\n");
    eprintln!("{text}");

    let dir = paths::data_dir().or_else(|_| Ok::<_, anyhow::Error>(std::env::temp_dir()));
    if let Ok(dir) = dir {
        let _ = std::fs::write(
            dir.join("startup-error.log"),
            format!("时间戳：{}\n\n{text}", now_epoch()),
        );
        eprintln!("（详情已写入 {}）", dir.join("startup-error.log").display());
    }

    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// 测试
//
// 筛选条件会被拼成动态 SQL，这是最容易出错也最不容易被测到的地方
// （类型系统管不着 SQL 字符串）。所以这里用真实的扫描链路造数据，
// 再把每个筛选维度都过一遍。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 造三次快门：两张 NEF + JPG 配对完整，一张只有 NEF。
    fn seeded(name: &str) -> (std::path::PathBuf, Arc<Mutex<Connection>>) {
        let dir = std::env::temp_dir().join(format!("sp-lib-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for i in ["0001", "0002"] {
            std::fs::write(dir.join(format!("DSC_{i}.NEF")), format!("nef-{i}")).unwrap();
            std::fs::write(dir.join(format!("DSC_{i}.JPG")), format!("jpg-{i}")).unwrap();
        }
        std::fs::write(dir.join("DSC_0003.NEF"), "nef-3").unwrap();

        let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));
        indexer::scan(&dir, &db).unwrap();
        (dir, db)
    }

    fn count(conn: &Arc<Mutex<Connection>>, filter: PairFilter) -> i64 {
        query_pairs(&conn.lock().unwrap(), &filter, 100, 0)
            .unwrap()
            .total
    }

    /// 换文件夹之后，视图必须只属于新文件夹。
    ///
    /// 旧文件夹的记录是**故意留着不清的**（标记按 pair_key 存，留着回头再选
    /// 同一个文件夹时标记会自己回来），所以这个范围条件是换文件夹不出错的唯一依靠。
    #[test]
    fn roots_scope_the_view_to_the_selected_folder() {
        let base = std::env::temp_dir().join(format!("sp-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let a = base.join("A");
        // 故意造一个前缀相近的兄弟目录：只比前缀的话 "…/A" 会把 "…/AB" 也算进来
        let ab = base.join("AB");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&ab).unwrap();

        for i in ["0001", "0002"] {
            std::fs::write(a.join(format!("DSC_{i}.NEF")), format!("nef-{i}")).unwrap();
            std::fs::write(a.join(format!("DSC_{i}.JPG")), format!("jpg-{i}")).unwrap();
        }
        std::fs::write(ab.join("DSC_0009.NEF"), "nef-9").unwrap();

        let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));
        indexer::scan_with_progress(&[a.clone(), ab.clone()], &db, |_| {}, None).unwrap();

        // 不限范围：两个目录都在，一共 3 张
        assert_eq!(count(&db, PairFilter::default()), 3);

        let a_str = a.to_string_lossy().to_string();
        let ab_str = ab.to_string_lossy().to_string();

        assert_eq!(
            count(
                &db,
                PairFilter {
                    roots: Some(vec![a_str.clone()]),
                    ..Default::default()
                }
            ),
            2,
            "选了 A 就只看到 A 的两张"
        );
        assert_eq!(
            count(
                &db,
                PairFilter {
                    roots: Some(vec![ab_str.clone()]),
                    ..Default::default()
                }
            ),
            1,
            "选了 AB 就只看到 AB 的一张，不能被 A 的前缀带进来"
        );

        // 侧栏计数也得跟着范围走，否则它报整库的数字、网格显示当前文件夹
        let f = facets_of(&db.lock().unwrap(), Some(&[a_str])).unwrap();
        assert_eq!(f.total, 2);
        assert_eq!(facets_of(&db.lock().unwrap(), None).unwrap().total, 3);
    }

    #[test]
    fn filters_by_pair_state() {
        let (dir, conn) = seeded("pairstate");
        assert_eq!(count(&conn, PairFilter::default()), 3, "不筛时是全部");
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    pair_state: Some("both".into()),
                    ..Default::default()
                }
            ),
            2
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    pair_state: Some("rawOnly".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    pair_state: Some("jpgOnly".into()),
                    ..Default::default()
                }
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    pair_state: Some("orphan".into()),
                    ..Default::default()
                }
            ),
            1,
            "「有缺失」应同时覆盖缺 NEF 与缺 JPG"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paging_keeps_total_independent_of_page_size() {
        let (dir, conn) = seeded("paging");
        // 前端靠 total 决定「还要不要继续往下加载」，所以它必须是完整计数
        let page = query_pairs(&conn.lock().unwrap(), &PairFilter::default(), 1, 0).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.total, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn searches_by_file_name() {
        let (dir, conn) = seeded("search");
        let page = query_pairs(
            &conn.lock().unwrap(),
            &PairFilter {
                search: Some("DSC_0002".into()),
                ..Default::default()
            },
            100,
            0,
        )
        .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.items[0].path.contains("DSC_0002"));

        assert_eq!(
            count(
                &conn,
                PairFilter {
                    search: Some("NOTHING-AT-ALL".into()),
                    ..Default::default()
                }
            ),
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `%` 和 `_` 都必须是字面量。不转义的话，`_` 在 LIKE 里是「任意一个字符」，
    /// 用户搜 `A_B` 会连带把 `AXB` 也找出来；`%` 更糟，直接匹配整个图库。
    #[test]
    fn search_treats_like_wildcards_literally() {
        let dir = std::env::temp_dir().join(format!("sp-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("A_B.NEF"), "x").unwrap();
        std::fs::write(dir.join("AXB.NEF"), "y").unwrap();

        let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));
        indexer::scan(&dir, &db).unwrap();

        let q = |s: &str| {
            count(
                &db,
                PairFilter {
                    search: Some(s.into()),
                    ..Default::default()
                },
            )
        };

        assert_eq!(q("A_B"), 1, "下划线应按字面量匹配，不该命中 AXB");
        assert_eq!(q("%"), 0, "百分号应按字面量匹配，路径里没有就是 0");
        assert_eq!(q("A"), 2, "对照：只搜 A 时两张都该命中");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sorts_take_effect() {
        let (dir, conn) = seeded("sort");
        let asc = query_pairs(
            &conn.lock().unwrap(),
            &PairFilter {
                sort: Some("nameAsc".into()),
                ..Default::default()
            },
            100,
            0,
        )
        .unwrap();
        let desc = query_pairs(
            &conn.lock().unwrap(),
            &PairFilter {
                sort: Some("nameDesc".into()),
                ..Default::default()
            },
            100,
            0,
        )
        .unwrap();
        assert!(asc.items[0].path < asc.items[2].path, "文件名升序");
        assert!(desc.items[0].path > desc.items[2].path, "文件名降序");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn facets_cover_every_dimension() {
        let (dir, conn) = seeded("facets");
        let f = facets_of(&conn.lock().unwrap(), None).unwrap();

        assert_eq!(f.total, 3);
        let pick = |key: &str| {
            f.pair_states
                .iter()
                .find(|x| x.key == key)
                .unwrap_or_else(|| panic!("分面里缺少 {key}"))
                .count
        };
        assert_eq!(pick("all"), 3);
        assert_eq!(pick("both"), 2);
        assert_eq!(pick("rawOnly"), 1);
        assert_eq!(pick("orphan"), 1);

        // 测试造的是纯文本假文件，读不到 EXIF，机身序列号应为空
        assert_eq!(f.cameras.len(), 1);
        assert_eq!(f.cameras[0].key, NONE_KEY);
        assert_eq!(f.cameras[0].count, 3);

        // 没有拍摄时间时会退回文件修改时间，所以仍能归到某一天
        assert_eq!(f.days.len(), 1);
        assert_eq!(f.days[0].count, 3);
        assert_eq!(f.days[0].key.len(), 10, "日期键应为 YYYY-MM-DD");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 选片
    // -----------------------------------------------------------------------

    /// 取默认顺序下的一页卡片。
    fn cards(conn: &Arc<Mutex<Connection>>, filter: PairFilter) -> Vec<PairCard> {
        query_pairs(&conn.lock().unwrap(), &filter, 100, 0)
            .unwrap()
            .items
    }

    /// 默认顺序下的第一张。
    fn first_card(conn: &Arc<Mutex<Connection>>) -> PairCard {
        cards(conn, PairFilter::default()).remove(0)
    }

    /// 只看选片状态 / 星级的筛选条件，其余维度不筛。
    fn by_mark(decision: Option<&str>, stars: Option<i64>) -> PairFilter {
        PairFilter {
            decision: decision.map(|s| s.to_string()),
            stars,
            ..Default::default()
        }
    }

    /// 只看色标。
    fn by_color(color: Option<&str>) -> PairFilter {
        PairFilter {
            color: color.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    fn stored_color(conn: &Arc<Mutex<Connection>>, pair_key: &str) -> String {
        conn.lock()
            .unwrap()
            .query_row(
                "SELECT color FROM decisions WHERE pair_key = ?1",
                [pair_key],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| String::new())
    }

    fn stored(conn: &Arc<Mutex<Connection>>, pair_key: &str) -> (String, i64) {
        conn.lock()
            .unwrap()
            .query_row(
                "SELECT decision, stars FROM decisions WHERE pair_key = ?1",
                [pair_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }

    /// 标记必须盖住整次快门。
    ///
    /// 如果只在被点中的那一行上打标记，就会出现「NEF 是保留、JPG 还是未标记」——
    /// 按状态筛的时候同一张照片出现两次、还给出矛盾的答案。
    #[test]
    fn marking_covers_the_whole_pair() {
        let (dir, conn) = seeded("pairmark");
        let card = first_card(&conn);
        let key = card.pair_key.clone();

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            Some(3),
            None,
        )
        .unwrap();

        // 直接看这个 pair 底下**每一个文件**拿到的状态
        let guard = conn.lock().unwrap();
        let mut stmt = guard
            .prepare(
                "SELECT p.file_kind, COALESCE(d.decision, 'none'), COALESCE(d.stars, 0)
                   FROM photos p LEFT JOIN decisions d ON d.pair_key = p.pair_key
                  WHERE p.pair_key = ?1",
            )
            .unwrap();
        let rows: Vec<(String, String, i64)> = stmt
            .query_map([&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        drop(stmt);

        assert_eq!(rows.len(), 2, "这一次快门应该有 NEF 和 JPG 两个文件");
        for (kind, decision, stars) in &rows {
            assert_eq!(decision, "keep", "{kind} 没跟上这次标记");
            assert_eq!(*stars, 3, "{kind} 的星级没跟上");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次标记里同时点到 NEF 和 JPG，只能算一张。
    #[test]
    fn marking_counts_pairs_not_files() {
        let (dir, conn) = seeded("dedupe");
        let key = first_card(&conn).pair_key.clone();

        let ids: Vec<i64> = {
            let guard = conn.lock().unwrap();
            let mut stmt = guard
                .prepare("SELECT id FROM photos WHERE pair_key = ?1")
                .unwrap();
            stmt.query_map([&key], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(ids.len(), 2);

        assert_eq!(
            apply_decision_rows(&mut conn.lock().unwrap(), &ids, Some("keep"), None, None).unwrap(),
            1,
            "同一张照片的两个文件应当合并成一次决定"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 标记必须活过一次重建索引。
    ///
    /// 这是整张表存在的理由。用「删光 photos 再重扫」来模拟最狠的情况：
    /// 所有自增 id 都变了，任何挂在 photo_id 上的东西都会跟着消失。
    #[test]
    fn marks_survive_a_full_reindex() {
        let (dir, conn) = seeded("survive");
        let card = first_card(&conn);
        let key = card.pair_key.clone();

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("reject"),
            Some(2),
            None,
        )
        .unwrap();
        assert_eq!(stored(&conn, &key), ("reject".to_string(), 2));

        conn.lock()
            .unwrap()
            .execute("DELETE FROM photos", [])
            .unwrap();
        indexer::scan(&dir, &conn).unwrap();

        assert_eq!(
            stored(&conn, &key),
            ("reject".to_string(), 2),
            "重扫把照片全部重建之后，标记必须还在"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只改星级不能把已经做好的保留/淘汰决定冲掉。
    #[test]
    fn changing_stars_keeps_the_decision() {
        let (dir, conn) = seeded("starspatch");
        let card = first_card(&conn);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            None,
            None,
        )
        .unwrap();
        // decision 传 None ＝ 这一项不改
        apply_decision_rows(&mut conn.lock().unwrap(), &[card.id], None, Some(5), None).unwrap();

        assert_eq!(
            stored(&conn, &card.pair_key),
            ("keep".to_string(), 5),
            "调星级不该动决定"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「清除」要把决定和星级一起清掉，不能留下「未标记但有三颗星」。
    #[test]
    fn clearing_resets_both_decision_and_stars() {
        let (dir, conn) = seeded("clearpatch");
        let card = first_card(&conn);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            Some(4),
            None,
        )
        .unwrap();
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("none"),
            Some(0),
            None,
        )
        .unwrap();

        assert_eq!(stored(&conn, &card.pair_key), ("none".to_string(), 0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 色标和决定、星级互相独立：改一个不该动另外两个。
    #[test]
    fn color_is_independent_of_decision_and_stars() {
        let (dir, conn) = seeded("colorsolo");
        let card = first_card(&conn);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            Some(2),
            Some("red"),
        )
        .unwrap();
        assert_eq!(stored_color(&conn, &card.pair_key), "red");
        assert_eq!(stored(&conn, &card.pair_key), ("keep".to_string(), 2));

        // decision / stars 传 None ＝ 这两项不动，只换色标
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            None,
            None,
            Some("green"),
        )
        .unwrap();
        assert_eq!(stored_color(&conn, &card.pair_key), "green", "只改色标");
        assert_eq!(
            stored(&conn, &card.pair_key),
            ("keep".to_string(), 2),
            "改色标不该顺手把决定和星级清掉"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「清掉色标」必须真的清掉。一个字段要表达设 / 清 / 不动三种意图，
    /// 最容易出的错就是把 Some("") 折成 None，结果永远清不掉。
    #[test]
    fn color_can_be_cleared_without_touching_other_marks() {
        let (dir, conn) = seeded("colorclear");
        let card = first_card(&conn);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            Some(3),
            Some("purple"),
        )
        .unwrap();
        // 色标要能单独筛出来
        assert_eq!(count(&conn, by_color(Some("purple"))), 1);
        assert_eq!(count(&conn, by_color(Some("red"))), 0);

        apply_decision_rows(&mut conn.lock().unwrap(), &[card.id], None, None, Some("")).unwrap();
        assert_eq!(
            stored_color(&conn, &card.pair_key),
            "",
            "传空串要真的把色标清掉"
        );
        assert_eq!(
            stored(&conn, &card.pair_key),
            ("keep".to_string(), 3),
            "清色标不该动决定和星级"
        );
        assert_eq!(count(&conn, by_color(Some("purple"))), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 色标认值走白名单：写脏数据不能让这张照片在 UI 上变得筛不到。
    #[test]
    fn unknown_color_values_are_ignored() {
        let (dir, conn) = seeded("colorjunk");
        let card = first_card(&conn);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            None,
            None,
            Some("chartreuse"),
        )
        .unwrap();
        assert_eq!(stored_color(&conn, &card.pair_key), "", "认不出就当没标");

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            None,
            None,
            Some("Red"),
        )
        .unwrap();
        assert_eq!(stored_color(&conn, &card.pair_key), "red", "大小写不敏感");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filters_by_decision_and_stars() {
        let (dir, conn) = seeded("decfilter");
        let page = cards(&conn, PairFilter::default());
        assert_eq!(page.len(), 3);

        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[page[0].id],
            Some("keep"),
            None,
            None,
        )
        .unwrap();
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[page[1].id],
            Some("reject"),
            Some(4),
            None,
        )
        .unwrap();

        let c = |f: PairFilter| count(&conn, f);
        assert_eq!(c(by_mark(None, None)), 3, "不筛时是全部");
        assert_eq!(c(by_mark(Some("keep"), None)), 1);
        assert_eq!(c(by_mark(Some("reject"), None)), 1);
        assert_eq!(c(by_mark(Some("none"), None)), 1);
        assert_eq!(
            c(by_mark(Some("marked"), None)),
            2,
            "「已标记」＝保留＋淘汰"
        );

        assert_eq!(c(by_mark(None, Some(4))), 1);
        // 「0 星」和「不筛星级」必须是两回事，否则「找没打星的」没法表达
        assert_eq!(c(by_mark(None, Some(0))), 2, "0 星要能单独筛出来");
        assert_eq!(c(by_mark(Some("keep"), Some(0))), 1, "两个维度要能叠加");

        // 认不出来的取值一律当「不筛」，不该悄悄把条件丢掉又看起来像生效了
        assert_eq!(c(by_mark(Some("随便什么"), None)), 3);
        // 导出对话框里的「当前筛选下的全部」就是靠这一条：传一个后端不认的值，
        // 于是不按选片状态筛，其余条件照旧生效
        assert_eq!(c(by_mark(Some("all"), None)), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 批量标记（「全选之后按 P」）要一次写完，且数量对得上。
    #[test]
    fn marking_a_whole_page_at_once() {
        let (dir, conn) = seeded("bulkmark");
        let ids: Vec<i64> = cards(&conn, PairFilter::default())
            .iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids.len(), 3);

        assert_eq!(
            apply_decision_rows(&mut conn.lock().unwrap(), &ids, Some("keep"), None, None).unwrap(),
            3
        );
        assert_eq!(count(&conn, by_mark(Some("keep"), None)), 3);
        assert_eq!(count(&conn, by_mark(Some("none"), None)), 0);

        // 空列表不该报错，也不该把谁标上
        assert_eq!(
            apply_decision_rows(&mut conn.lock().unwrap(), &[], Some("reject"), None, None)
                .unwrap(),
            0
        );
        assert_eq!(count(&conn, by_mark(Some("reject"), None)), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn facets_report_decision_and_star_counts() {
        let (dir, conn) = seeded("decfacets");
        let page = cards(&conn, PairFilter::default());
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[page[0].id],
            Some("keep"),
            Some(4),
            None,
        )
        .unwrap();

        let f = facets_of(&conn.lock().unwrap(), None).unwrap();
        let pick = |list: &[Facet], key: &str| {
            list.iter()
                .find(|x| x.key == key)
                .unwrap_or_else(|| panic!("分面里缺少 {key}"))
                .count
        };

        assert_eq!(pick(&f.decisions, "all"), 3);
        assert_eq!(pick(&f.decisions, "keep"), 1);
        assert_eq!(pick(&f.decisions, "reject"), 0);
        assert_eq!(pick(&f.decisions, "none"), 2);

        assert_eq!(f.stars.len(), 6, "星级固定六档，位置不随数据跳动");
        assert_eq!(pick(&f.stars, "0"), 2);
        assert_eq!(pick(&f.stars, "4"), 1);
        assert_eq!(pick(&f.stars, "5"), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 导出
    // -----------------------------------------------------------------------

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("sp-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// 数一行 CSV 有几个字段。引号里的逗号不算——
    /// 照片文件名带逗号是常事，靠 `split(',')` 数会数错。
    fn count_fields(line: &str) -> usize {
        let mut n = 1;
        let mut in_quotes = false;
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' if !in_quotes => in_quotes = true,
                '"' if in_quotes => {
                    if chars.clone().next() == Some('"') {
                        chars.next(); // 翻倍的引号＝一个字面引号，仍在引号内部
                    } else {
                        in_quotes = false;
                    }
                }
                ',' if !in_quotes => n += 1,
                _ => {}
            }
        }
        n
    }

    #[test]
    fn csv_escapes_commas_and_quotes() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("he said \"hi\""), "\"he said \"\"hi\"\"\"");
        assert_eq!(csv_field("line1\nline2"), "\"line1\nline2\"");
    }

    /// 清单的每一行都必须和表头一样宽，否则表格整体错位——
    /// 而错位的表格比没有表格更糟，你会照着它去删照片。
    #[test]
    fn manifest_rows_line_up_with_the_header() {
        let pair = ExportPair {
            pair_key: "/tmp/a/dsc_0001".into(),
            decision: "keep".into(),
            stars: 3,
            taken_at_text: Some("2026-09-16 11:20:30".into()),
            camera_model: Some("NIKON Z50_2".into()),
            lens: Some("NIKKOR Z 24-120mm".into()),
            focal_len: Some(50.0),
            aperture: Some(2.8),
            shutter: Some("1/500".into()),
            iso: Some(200),
            files: vec![
                // 故意用带逗号和引号的文件名，逼出转义路径
                ExportFile {
                    path: "/tmp/a/MY, SHOT \"x\".NEF".into(),
                    size: 100,
                },
                ExportFile {
                    path: "/tmp/a/b.JPG".into(),
                    size: 50,
                },
            ],
        };
        let missing = ExportPair {
            pair_key: "/tmp/a/ghost".into(),
            decision: "none".into(),
            stars: 0,
            taken_at_text: None,
            camera_model: None,
            lens: None,
            focal_len: None,
            aperture: None,
            shutter: None,
            iso: None,
            files: Vec::new(), // 索引里有、磁盘上找不到
        };

        let csv = manifest_csv(&[pair, missing]);
        let body = csv.trim_start_matches('\u{feff}');
        let mut lines = body.lines();
        let header_cols = count_fields(lines.next().unwrap());
        assert_eq!(header_cols, 11);

        for line in lines {
            assert_eq!(count_fields(line), header_cols, "列数对不上：{line}");
        }

        // 磁盘上找不到的那张也必须出现在清单里——悄悄漏掉它，
        // 用户会以为这一张已经处理过了
        assert!(body.contains("(文件缺失)"));
        assert!(
            body.contains("\"MY, SHOT \"\"x\"\".NEF\""),
            "带逗号引号的文件名要正确转义"
        );
    }

    #[test]
    fn export_copies_files_and_writes_a_manifest() {
        let (src_dir, conn) = seeded("export");
        // 挑一张配对完整的，才能验证「NEF 和 JPG 都被复制」
        let card = query_pairs(
            &conn.lock().unwrap(),
            &PairFilter {
                pair_state: Some("both".into()),
                ..Default::default()
            },
            1,
            0,
        )
        .unwrap()
        .items
        .remove(0);
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            Some(5),
            None,
        )
        .unwrap();

        let dest = temp_dir("export-dest");
        let filter = by_mark(Some("keep"), None);

        let s = export_rows(
            &conn.lock().unwrap(),
            &filter,
            &dest,
            true,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();
        assert_eq!(s.photos, 1);
        assert_eq!(s.files, 2, "NEF 和 JPG 都要复制");
        assert_eq!(s.copied, 2);
        assert_eq!(s.failed, 0);
        assert_eq!(s.skipped, 0);

        let csv = std::fs::read_to_string(&s.manifest).unwrap();
        assert!(
            csv.starts_with('\u{feff}'),
            "要有 BOM，否则 Windows 版 Excel 读成乱码"
        );
        assert!(csv.contains("保留,5,"), "清单里要有决定和星级");

        // 再导一遍：不该滚出一堆 -1 -2 的副本
        let again = export_rows(
            &conn.lock().unwrap(),
            &filter,
            &dest,
            true,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();
        assert_eq!(again.copied, 0);
        assert_eq!(again.skipped, 2, "同名同大小的文件应当跳过");

        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    /// 「只出清单」模式一个文件都不该复制。
    #[test]
    fn list_only_mode_touches_no_files() {
        let (src_dir, conn) = seeded("export-list");
        let card = first_card(&conn);
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[card.id],
            Some("keep"),
            None,
            None,
        )
        .unwrap();

        let dest = temp_dir("export-list-dest");
        let s = export_rows(
            &conn.lock().unwrap(),
            &by_mark(Some("keep"), None),
            &dest,
            false,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();

        assert_eq!(s.copied, 0);
        assert_eq!(s.bytes, 0);
        assert!(std::path::Path::new(&s.manifest).exists(), "清单还是要写");

        // 目标文件夹里除了清单不该有别的
        let files: Vec<String> = std::fs::read_dir(&dest)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files, vec!["选片清单.csv".to_string()]);

        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    /// 导出绝不能碰原片。这条是硬底线，所以用一个明确的断言守着。
    /// 画面分析：清晰度与曝光。
    ///
    /// 直接测到「造一张糊图 → 能被筛出来」这一层，因为这个功能最容易写反的地方
    /// 不是公式，而是「算完没写库」或「写了但筛选用的是另一套阈值」。
    #[test]
    fn analysis_fills_metrics_and_quality_filter_picks_them_up() {
        let dir = std::env::temp_dir().join(format!("sp-analyze-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 造两张「NEF」：前面塞一段假头部，后面接一张真 JPEG 当内嵌预览。
        // 一张高频噪点（清晰），一张纯灰（糊）。
        let sharp = {
            // 棋盘格：缩到 256 之后边缘还在，才是真的「清晰」。
            // 高频噪点会被缩放平均成一片灰，用来当样本会得出相反结论。
            let mut img = image::GrayImage::new(800, 600);
            for (x, y, p) in img.enumerate_pixels_mut() {
                let on = (x / 16 + y / 16) % 2 == 0;
                *p = image::Luma([if on { 220 } else { 30 }]);
            }
            img
        };
        let blurry = image::GrayImage::from_pixel(800, 600, image::Luma([128u8]));

        std::fs::write(
            dir.join("DSC_0001.NEF"),
            fake_nef(&image::DynamicImage::ImageLuma8(sharp)),
        )
        .unwrap();
        std::fs::write(
            dir.join("DSC_0002.NEF"),
            fake_nef(&image::DynamicImage::ImageLuma8(blurry)),
        )
        .unwrap();

        let db = Arc::new(Mutex::new(db::open_in_memory().unwrap()));
        indexer::scan(&dir, &db).unwrap();
        assert_eq!(count(&db, PairFilter::default()), 2);

        // 分析之前：三档都是 0，不能被当成「没问题」之外的任何结论
        assert_eq!(count(&db, by_quality("blur")), 0, "没分析过就不该被判成糊");

        let roots = vec![dir.to_string_lossy().to_string()];
        let s = analyze::analyze_pending(
            &roots
                .iter()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>(),
            &db,
            |_| {},
        )
        .unwrap();
        assert_eq!(s.analyzed, 2, "两张都该算出来：{s:?}");
        assert_eq!(s.remaining, 0);

        // 糊的那张要能被筛出来，清晰的那张不能
        assert_eq!(count(&db, by_quality("blur")), 1, "只有一张是糊的");

        // 再跑一趟：已经算过的不该重复算
        let again = analyze::analyze_pending(
            &roots
                .iter()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>(),
            &db,
            |_| {},
        )
        .unwrap();
        assert_eq!(again.analyzed, 0, "算过的就不该再算");

        // 排序也要跟着走：清晰度低→高时，糊的那张排第一
        let first = query_pairs(
            &db.lock().unwrap(),
            &PairFilter {
                sort: Some("sharpAsc".into()),
                ..Default::default()
            },
            10,
            0,
        )
        .unwrap()
        .items;
        assert_eq!(first.len(), 2);
        assert!(
            first[0].sharpness.unwrap_or(0.0) <= first[1].sharpness.unwrap_or(0.0),
            "低→高排序：{:?} 该排在 {:?} 前",
            first[0].sharpness,
            first[1].sharpness
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 镜头 / 焦段 / ISO：这三个维度的取值是 EXIF 里的连续量，
    /// 最容易错的地方是档位边界（24 / 70 / 200 到底归哪一档、会不会漏张）。
    #[test]
    fn lens_focal_and_iso_filters_respect_their_buckets() {
        let (dir, conn) = seeded("params");
        {
            let c = conn.lock().unwrap();
            // 三张照片分别落在不同档位上，边界值故意取在档位的分界点
            c.execute(
                "UPDATE photos SET lens = 'NIKKOR Z 24-70', focal_len = 24, iso = 400 WHERE path LIKE '%0001.NEF'",
                [],
            )
            .unwrap();
            c.execute(
                "UPDATE photos SET lens = 'NIKKOR Z 70-200', focal_len = 199, iso = 1600 WHERE path LIKE '%0002.NEF'",
                [],
            )
            .unwrap();
            c.execute(
                "UPDATE photos SET lens = NULL, focal_len = 300, iso = 12800 WHERE path LIKE '%0003.NEF'",
                [],
            )
            .unwrap();
        }

        // 镜头：按型号筛，读不到镜头的那张归到「未知」
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    lens: Some("NIKKOR Z 24-70".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    lens: Some(NONE_KEY.into()),
                    ..Default::default()
                }
            ),
            1,
            "没有镜头信息的该能单独筛出来"
        );

        // 焦段：24 属于「24–70」而不是「24 以下」，200 以上归 super
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    focal: Some("wide".into()),
                    ..Default::default()
                }
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    focal: Some("normal".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    focal: Some("tele".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    focal: Some("super".into()),
                    ..Default::default()
                }
            ),
            1
        );

        // ISO：400 落在 mid，不是 low
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    iso: Some("low".into()),
                    ..Default::default()
                }
            ),
            0
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    iso: Some("mid".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    iso: Some("high".into()),
                    ..Default::default()
                }
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                PairFilter {
                    iso: Some("veryHigh".into()),
                    ..Default::default()
                }
            ),
            1
        );

        // 档位加起来要等于总数，不许有漏在档外的
        let buckets = ["wide", "normal", "tele", "super"]
            .iter()
            .map(|k| {
                count(
                    &conn,
                    PairFilter {
                        focal: Some(k.to_string()),
                        ..Default::default()
                    },
                )
            })
            .sum::<i64>();
        assert_eq!(buckets, 3, "所有照片都该落在某一档里");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn by_quality(q: &str) -> PairFilter {
        PairFilter {
            quality: Some(q.to_string()),
            ..Default::default()
        }
    }

    /// 把一张图包成「NEF」：假头部 + 真 JPEG。preview 抠取看的是 JPEG 标记，
    /// 这么造出来的文件足以走通整条分析链路。
    fn fake_nef(img: &image::DynamicImage) -> Vec<u8> {
        let mut jpeg: Vec<u8> = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut jpeg),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        let mut out = b"NIKON CORPORATION FAKE NEF HEADER \0\0\0".to_vec();
        out.extend_from_slice(&jpeg);
        out
    }

    #[test]
    fn export_can_be_limited_to_an_explicit_set_of_photos() {
        let (src_dir, conn) = seeded("export-ids");
        let all = query_pairs(&conn.lock().unwrap(), &PairFilter::default(), 100, 0)
            .unwrap()
            .items;
        assert!(all.len() >= 3, "样本里至少要有三张才能看出筛没筛");

        // 只导出中间那一张：勾了什么就是什么，不看选片状态、也不看别的条件
        let wanted = all[1].id;
        let dest = temp_dir("export-ids-dest");
        let s = export_rows(
            &conn.lock().unwrap(),
            &PairFilter {
                ids: Some(vec![wanted]),
                ..Default::default()
            },
            &dest,
            true,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();
        assert_eq!(s.photos, 1, "只该导出指定的那一组");
        assert!(!s.manifest.is_empty());

        // 顺带确认 ids 与选片状态无关：这张没标过记，导出照样成立
        let by_mark_count = query_pairs(
            &conn.lock().unwrap(),
            &PairFilter {
                ids: Some(vec![wanted]),
                decision: Some("keep".into()),
                ..Default::default()
            },
            100,
            0,
        )
        .unwrap()
        .total;
        assert_eq!(by_mark_count, 0, "叠加条件时该取交集，不是只看 ids");

        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn export_never_modifies_the_originals() {
        let (src_dir, conn) = seeded("export-readonly");

        // 两种状态都标上，好把「保留」和「淘汰」两条导出路径都跑一遍——
        // 淘汰恰恰是最容易被谁写成删除的那个分支
        let page = cards(&conn, PairFilter::default());
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[page[0].id],
            Some("reject"),
            None,
            None,
        )
        .unwrap();
        apply_decision_rows(
            &mut conn.lock().unwrap(),
            &[page[1].id],
            Some("keep"),
            None,
            None,
        )
        .unwrap();

        let snapshot = |dir: &std::path::Path| -> Vec<(String, u64)> {
            let mut v: Vec<(String, u64)> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| {
                    (
                        e.file_name().to_string_lossy().to_string(),
                        e.metadata().unwrap().len(),
                    )
                })
                .collect();
            v.sort();
            v
        };

        let before = snapshot(&src_dir);
        assert_eq!(before.len(), 5, "三次快门：2 对完整 + 1 张孤立 NEF");

        let dest = temp_dir("export-ro-dest");
        export_rows(
            &conn.lock().unwrap(),
            &by_mark(None, None),
            &dest,
            true,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();
        export_rows(
            &conn.lock().unwrap(),
            &by_mark(Some("reject"), None),
            &dest,
            true,
            FileScope::Both,
            DEFAULT_NAME_TEMPLATE,
            |_| {},
        )
        .unwrap();

        assert_eq!(
            before,
            snapshot(&src_dir),
            "原片目录的文件集合和大小都不该有任何变化"
        );

        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::remove_dir_all(&src_dir);
    }

    #[test]
    fn plan_target_is_repeatable_and_never_overwrites() {
        let dir = temp_dir("plan");

        // 目标里没有同名文件 → 直接用原名
        let first = plan_target(&dir, "A.NEF", 10).unwrap();
        assert_eq!(first.file_name().unwrap(), "A.NEF");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&first, vec![b'x'; 10]).unwrap();

        // 同名同大小 → 跳过，重复导出不会滚副本
        assert!(plan_target(&dir, "A.NEF", 10).is_none());

        // 真的撞名（同名但大小不同）→ 让位到 -1，不能覆盖
        let second = plan_target(&dir, "A.NEF", 11).unwrap();
        assert_eq!(second.file_name().unwrap(), "A-1.NEF");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 「清缓存」最怕的就是把人辛苦选了一晚上的结果一起清掉。
    /// 这一档必须保证：索引没了，标记还在。
    #[test]
    fn sizes_for_picks_the_right_tier() {
        // 放大到 100% 要的是高清档，不能因为分派写错退回到 512
        assert_eq!(sizes_for(thumb::SIZE_PREVIEW), vec![thumb::SIZE_PREVIEW]);
        assert_eq!(sizes_for(thumb::SIZE_LOUPE), vec![thumb::SIZE_LOUPE]);
        // 网格顺带把 micro 一起建了：同一次解码，多一档几乎不要钱
        assert_eq!(
            sizes_for(thumb::SIZE_GRID),
            vec![thumb::SIZE_GRID, thumb::SIZE_MICRO]
        );
        assert!(
            thumb::SIZE_PREVIEW > thumb::SIZE_LOUPE,
            "高清档必须比大图档大"
        );
    }

    #[test]
    fn cluster_by_gap_splits_at_the_boundary() {
        // 间隔「不超过」gap 算同一组：差 3 秒还在组内，差 4 秒就断开
        let times = [0, 2, 5, 9, 30];
        let groups = cluster_by_gap(&times, 3);
        assert_eq!(groups, vec![vec![0, 1, 2], vec![3], vec![4]]);
    }

    #[test]
    fn cluster_by_gap_handles_empty_and_single() {
        assert!(cluster_by_gap(&[], 3).is_empty());
        assert_eq!(cluster_by_gap(&[42], 3), vec![vec![0]]);
    }

    #[test]
    fn render_name_fills_variables() {
        let ctx = NameCtx {
            stem: "DSC_0001",
            ext: "NEF",
            date: "20260916",
            time: "102030",
            seq: 7,
            stars: 3,
            camera: "NIKON Z 50II",
            pair: "key-1",
        };
        // 默认模板等于保持原文件名
        assert_eq!(render_name("{name}", &ctx), "DSC_0001.NEF");
        assert_eq!(
            render_name("{date}_{seq}_{name}", &ctx),
            "20260916_0007_DSC_0001.NEF"
        );
        assert_eq!(render_name("{stars}星_{pair}", &ctx), "3星_key-1.NEF");
        // 模板里没写 {ext} 也会补上，不会导出一堆没有扩展名的文件
        assert!(render_name("{date}-{seq}", &ctx).ends_with(".NEF"));
    }

    #[test]
    fn render_name_cleans_illegal_chars_and_empty_results() {
        let ctx = NameCtx {
            stem: "a/b",
            ext: "nef",
            date: "",
            time: "",
            seq: 1,
            stars: 0,
            camera: "NIKON: Z/50",
            pair: "k",
        };
        // 文件名里的非法字符必须换掉，否则 Windows 上直接写失败
        let out = render_name("{camera}_{name}", &ctx);
        assert!(!out.contains('/') && !out.contains(':'));
        // 空模板也不能产出「只有扩展名」的文件
        assert_ne!(render_name("", &ctx), ".nef");
    }

    #[test]
    fn file_scope_picks_only_the_asked_kind() {
        assert!(FileScope::Both.keeps("raw") && FileScope::Both.keeps("jpeg"));
        assert!(FileScope::Raw.keeps("raw") && !FileScope::Raw.keeps("jpeg"));
        assert!(FileScope::Jpeg.keeps("jpeg") && !FileScope::Jpeg.keeps("raw"));
        // 认不出的取值退回「都要」，宁可多导也不能什么都不导
        assert_eq!(FileScope::parse("whatever"), FileScope::Both);
    }

    #[test]
    fn clearing_index_keeps_decisions() {
        let mut conn = db::open_in_memory().unwrap();
        conn.execute(
            "INSERT INTO photos(path, pair_key) VALUES('/x/DSC_0001.NEF', '/x/DSC_0001')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO decisions(pair_key, decision, stars) VALUES('/x/DSC_0001', 'keep', 3)",
            [],
        )
        .unwrap();

        purge_index(&mut conn, "index").unwrap();

        let photos: i64 = conn
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(photos, 0, "索引应当被清干净");

        let kept: (String, i64) = conn
            .query_row(
                "SELECT decision, stars FROM decisions WHERE pair_key = '/x/DSC_0001'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("清索引不能连选片标记一起清掉");
        assert_eq!(kept, ("keep".to_string(), 3));

        // 只有「全部清空」才会动标记
        purge_index(&mut conn, "all").unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM decisions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);

        // 只清缩略图那一档：数据库必须原封不动
        let mut fresh = db::open_in_memory().unwrap();
        fresh
            .execute(
                "INSERT INTO photos(path, pair_key) VALUES('/y/a.NEF', '/y/a')",
                [],
            )
            .unwrap();
        purge_index(&mut fresh, "thumbs").unwrap();
        let still: i64 = fresh
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(still, 1, "只清缩略图时不该碰数据库");
    }

    #[test]
    fn photo_detail_returns_full_row_and_sibling_existence() {
        let conn = db::open_in_memory().unwrap();
        let dir = temp_dir("detail");
        std::fs::create_dir_all(&dir).unwrap();
        let jpg = dir.join("DSC_0001.JPG");
        std::fs::write(&jpg, b"jpeg").unwrap();

        conn.execute(
            "INSERT INTO photos(path, pair_key, file_kind, is_primary, file_size, width, height,
                                taken_at_corrected, camera_model, camera_serial, lens, focal_len,
                                aperture, shutter, iso, orientation, exif_ok, indexed_at,
                                sharpness, overexposed, underexposed, fingerprint)
             VALUES(?1, '/d/DSC_0001', 'raw', 1, 24000000, 6000, 4000, 1787000000,
                    'Nikon Z 50II', '8069200', 'NIKKOR Z 24-70mm', 35.0, 2.8, '1/250', 400, 1, 1,
                    strftime('%s','now'), 87.5, 0.0, 0.0, 'fp-0001')",
            [dir.join("DSC_0001.NEF").to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO photos(path, pair_key, file_kind, is_primary, file_size)
             VALUES(?1, '/d/DSC_0001', 'jpeg', 0, 8000000)",
            [jpg.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO decisions(pair_key, decision, stars) VALUES('/d/DSC_0001', 'keep', 4)",
            [],
        )
        .unwrap();

        let nef_id: i64 = conn
            .query_row("SELECT id FROM photos WHERE file_kind = 'raw'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let d = photo_detail_of(&conn, nef_id).unwrap();
        assert_eq!(d.file_name, "DSC_0001.NEF");
        assert_eq!(d.camera_model.as_deref(), Some("Nikon Z 50II"));
        assert_eq!(d.decision, "keep");
        assert_eq!(d.stars, 4);
        assert_eq!(d.time_source, "exif");
        assert!(
            d.time_text.as_deref().unwrap().starts_with("2026-"),
            "时间文本要和侧栏同一套语义"
        );
        assert_eq!(d.siblings.len(), 1, "同一次快门的 JPG 要作为同组文件出现");
        assert_eq!(d.siblings[0].file_kind, "jpeg");
        assert!(d.siblings[0].exists, "JPG 实际存在，必须报存在");

        // 把 JPG 删掉再查：exists 要如实翻转——库存里有不代表文件还在
        std::fs::remove_file(&jpg).unwrap();
        let d2 = photo_detail_of(&conn, nef_id).unwrap();
        assert!(!d2.siblings[0].exists, "文件已挪走还报存在就是误导");

        // 查不存在的 id 要报错：详情面板对空值明确报错，而不是画一屏空行
        assert!(photo_detail_of(&conn, -1).is_err());
    }

    #[test]
    fn purge_empties_the_directory_but_keeps_it() {
        let dir = temp_dir("purge");
        std::fs::create_dir_all(dir.join("ab")).unwrap();
        std::fs::write(dir.join("ab").join("x-512.jpg"), b"jpeg").unwrap();
        std::fs::write(dir.join("loose.tmp"), b"half").unwrap();

        purge_dir_contents(&dir).unwrap();

        assert!(dir.is_dir(), "目录本身要留着，省得下次重建");
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "里面的文件和子目录都该被清掉"
        );

        // 目录不存在也不能报错——用户可能从没看过缩略图
        let missing = temp_dir("purge-missing");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(purge_dir_contents(&missing).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
