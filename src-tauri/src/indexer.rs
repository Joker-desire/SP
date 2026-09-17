//! 索引器：扫描 → 配对 → EXIF → 指纹 → 入库。
//!
//! 性能要点（详见方案文档第七、九节）：
//! - **绝不做全文件哈希**。一张 NEF 三四十 MB，几千张就是上百 GB 的读取量。
//!   用「文件大小 + 修改时间 + 前 1MB 内容哈希」作为指纹，快 50-100 倍。
//! - EXIF 解析与指纹计算走 rayon 并行，DB 写入单线程包事务。

use anyhow::{anyhow, Context, Result};
use rayon::prelude::*;
use rusqlite::{params, Connection, Transaction};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

use crate::pairing::{self, FileKind};

/// 指纹只读文件头部这么多字节，够用且极快。
const FINGERPRINT_HEAD_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default, Clone)]
struct PhotoRow {
    path: String,
    fingerprint: String,
    pair_key: String,
    file_kind: String,
    file_size: i64,
    mtime: i64,
    width: Option<i64>,
    height: Option<i64>,
    taken_at: Option<i64>,
    camera_model: Option<String>,
    camera_serial: Option<String>,
    lens: Option<String>,
    focal_len: Option<f64>,
    aperture: Option<f64>,
    shutter: Option<String>,
    iso: Option<i64>,
    orientation: Option<i64>,
    exif_ok: i64,
}

enum Parsed {
    Unchanged(String),
    Fresh(Box<PhotoRow>),
    Failed(String),
}

/// 已入库文件的轻量快照，用来判断「这个文件还需要重新解析吗」。
///
/// 带上 `file_size` / `mtime` 是为了一遍 **stat 就能跳过**：
/// 只存指纹的话，每次增量扫描都得把每个文件的前 1MB 读出来算哈希——
/// 几千张就是几个 GB 的无谓读取，而它们 99.9% 都没变过。
#[derive(Clone)]
struct KnownFile {
    fingerprint: String,
    file_size: i64,
    mtime: i64,
}

/// 扫描进度。前端据此显示进度条——几千张的首次扫描要跑几分钟，
/// 没有反馈的话用户会以为程序卡死了。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// walking（遍历目录）/ parsing（读元数据）/ writing（写库）/ stats（统计）
    pub phase: &'static str,
    pub done: usize,
    pub total: usize,
}

impl ScanProgress {
    fn new(phase: &'static str, done: usize, total: usize) -> Self {
        Self { phase, done, total }
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    /// 磁盘上识别到的文件数（NEF + JPG 都算）
    pub scanned: usize,
    /// 指纹未变、直接跳过的文件数
    pub unchanged: usize,
    /// 新增入库的文件数
    pub inserted: usize,
    /// 已存在但内容有变、被更新的文件数
    pub updated: usize,
    /// 已从磁盘移走、被清理的记录数
    pub removed: usize,
    /// 读不到元数据而跳过的文件数
    pub failed: usize,
    /// 配对后的「照片」数（一次快门算一张）
    pub pairs: usize,
    /// 只有 RAW 没有 JPG 的配对数
    pub orphan_raw: usize,
    /// 只有 JPG 没有 RAW 的配对数
    pub orphan_jpg: usize,
    /// EXIF 解析失败的文件数（仍会入库，只是元数据缺失）
    pub exif_failed: usize,
    pub elapsed_ms: u128,
}

/// 扫描入口，不上报进度。
///
/// 真正的调用方（`scan_folder` 命令）一律走 `scan_with_progress`；
/// 这个包装只剩测试在用——测试关心的是「扫出什么结果」，不是进度条。
#[cfg(test)]
pub fn scan(root: &Path, db: &Arc<Mutex<Connection>>) -> Result<ScanSummary> {
    scan_with_progress(&[root.to_path_buf()], db, |_| {}, None)
}

/// 扫描并流式上报进度。
///
/// 阶段划分是有意的：遍历目录 → 并行读元数据 → 分批写库 → 统计。
///
/// **分批提交**是关键改动：每解析出一批（默认 200 张）就开一个事务写进去并提交，
/// 而不是等全部解析完再开一个巨型事务。这样前端在扫描进行中就能反复来
/// `list_pairs` 查到「已经入库的那部分」，做到「先展示前 N 张、边看边等」。
/// 事务之间会释放数据库连接锁，前端的查询不会被饿死。
///
/// `roots` 是「要扫描哪些目录」——可以是用户勾选的若干子文件夹，而不是
/// 永远递归整个根目录（见文件夹范围选择）。
///
/// `max_depth` 用来表达「只读这一层」：文件夹里既有照片又有子文件夹、用户一个
/// 子文件夹都不勾时，就按 `Some(1)` 扫，只把根目录自己的照片读出来。
/// `None` 表示不限层数。
pub fn scan_with_progress<F>(
    roots: &[PathBuf],
    db: &Arc<Mutex<Connection>>,
    on_progress: F,
    max_depth: Option<usize>,
) -> Result<ScanSummary>
where
    F: Fn(ScanProgress) + Send + Sync,
{
    let t0 = std::time::Instant::now();
    let mut summary = ScanSummary::default();

    if roots.is_empty() || roots.iter().all(|r| !r.is_dir()) {
        return Err(anyhow!(
            "目录不存在或不是文件夹：{}",
            roots
                .first()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        ));
    }

    // ---- 1) 收集候选文件（遍历用户勾选的每个目录）----
    on_progress(ScanProgress::new("walking", 0, 0));
    let mut files: Vec<PathBuf> = Vec::new();
    for r in roots {
        let walker = WalkDir::new(r).follow_links(false);
        let it = match max_depth {
            Some(d) => walker.max_depth(d).into_iter(),
            None => walker.into_iter(),
        };
        for entry in it {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            if pairing::classify(&pairing::ext_lower(p)).is_none() {
                continue;
            }
            files.push(p.to_path_buf());
        }
    }
    summary.scanned = files.len();
    let total = files.len();
    if total == 0 {
        // 一个文件都没有：没有可写的东西，但统计阶段仍要跑（让前端拿到 0 张）。
        on_progress(ScanProgress::new("stats", 0, 1));
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        summarize(&conn, &mut summary)?;
        summary.elapsed_ms = t0.elapsed().as_millis();
        return Ok(summary);
    }

    // ---- 2) 已入库的 路径 -> 快照 ----
    let mut known: HashMap<String, KnownFile> = HashMap::new();
    {
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT path, fingerprint, file_size, mtime FROM photos")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            known.insert(
                r.get(0)?,
                KnownFile {
                    fingerprint: r.get(1)?,
                    file_size: r.get(2)?,
                    mtime: r.get(3)?,
                },
            );
        }
    }

    // ---- 3) 机身时间偏移（用于 taken_at_corrected）----
    let mut offsets: HashMap<String, i64> = HashMap::new();
    {
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        let mut stmt = conn.prepare("SELECT serial, time_offset_seconds FROM camera_bodies")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            offsets.insert(r.get(0)?, r.get(1)?);
        }
    }

    // ---- 4) 并行解析，结果经 channel 流式回主线程 ----
    //
    // 不用 `.collect()` 一次拿全，而是每解析完一张就 send 出来。主线程边收边
    // 攒批写库，所以第一批（比如前 200 张）一落库，前端就能查到并开始显示，
    // 不用傻等几千张全部解析完。rayon 负责把解析平摊到所有核。
    on_progress(ScanProgress::new("parsing", 0, total));
    let (tx, rx) = mpsc::channel::<Parsed>();
    {
        let known = &known;
        let worker = tx.clone();
        drop(tx);
        files.par_iter().for_each(|p| {
            let _ = worker.send(parse_one(p, known));
        });
        // worker 的克隆在 for_each 结束（所有线程 join）后随闭包一起丢弃，
        // 加上上面 drop 掉的原 sender，channel 在此关闭，主线程的 for 循环得以结束。
    }

    // ---- 5) 边收边写：每 BATCH 张提交一次事务 ----
    const BATCH: usize = 200;
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch_rows: Vec<PhotoRow> = Vec::new();
    let mut batch_keys: Vec<String> = Vec::new();
    let mut parsed = 0usize;
    let mut written = 0usize;

    // 闭包：把攒好的一批写进库并提交，同时把这批涉及 pair 的主文件标记算对。
    // 每次调用都重新取锁/放锁，所以前端在批与批之间能插进来查询。
    let commit = |batch_rows: &mut Vec<PhotoRow>,
                  batch_keys: &mut Vec<String>,
                  summary: &mut ScanSummary|
     -> Result<()> {
        if batch_rows.is_empty() {
            return Ok(());
        }
        let mut conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        let tx = conn.transaction()?;
        {
            let mut ins = tx.prepare(
                "INSERT INTO photos (
                    path, fingerprint, pair_key, file_kind, file_size, mtime,
                    width, height, taken_at, taken_at_corrected,
                    camera_model, camera_serial, lens, focal_len, aperture, shutter,
                    iso, orientation, exif_ok, indexed_at
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
                 ON CONFLICT(path) DO UPDATE SET
                    fingerprint        = excluded.fingerprint,
                    pair_key           = excluded.pair_key,
                    file_kind          = excluded.file_kind,
                    file_size          = excluded.file_size,
                    mtime              = excluded.mtime,
                    width              = excluded.width,
                    height             = excluded.height,
                    taken_at           = excluded.taken_at,
                    taken_at_corrected = excluded.taken_at_corrected,
                    camera_model       = excluded.camera_model,
                    camera_serial      = excluded.camera_serial,
                    lens                = excluded.lens,
                    focal_len          = excluded.focal_len,
                    aperture           = excluded.aperture,
                    shutter            = excluded.shutter,
                    iso                 = excluded.iso,
                    orientation         = excluded.orientation,
                    exif_ok             = excluded.exif_ok,
                    indexed_at          = excluded.indexed_at",
            )?;

            for r in batch_rows.iter() {
                let corrected = r.taken_at.map(|t| {
                    let off = r
                        .camera_serial
                        .as_deref()
                        .and_then(|s| offsets.get(s))
                        .copied()
                        .unwrap_or(0);
                    t + off
                });
                ins.execute(params![
                    r.path,
                    r.fingerprint,
                    r.pair_key,
                    r.file_kind,
                    r.file_size,
                    r.mtime,
                    r.width,
                    r.height,
                    r.taken_at,
                    corrected,
                    r.camera_model,
                    r.camera_serial,
                    r.lens,
                    r.focal_len,
                    r.aperture,
                    r.shutter,
                    r.iso,
                    r.orientation,
                    r.exif_ok,
                    now_epoch(),
                ])?;
                if known.contains_key(&r.path) {
                    summary.updated += 1;
                } else {
                    summary.inserted += 1;
                }
            }

            // 主文件标记：同一 pair 内 RAW 优先。只重算这批涉及到的 pair，
            // 既正确又能让前端在写入当下就能按 is_primary 查到。
            set_primary_for(&tx, batch_keys)?;
        }
        tx.commit()?;
        batch_rows.clear();
        batch_keys.clear();
        Ok(())
    };

    for o in rx {
        match o {
            Parsed::Unchanged(p) => {
                summary.unchanged += 1;
                seen.insert(p);
            }
            Parsed::Fresh(r) => {
                seen.insert(r.path.clone());
                batch_keys.push(r.pair_key.clone());
                batch_rows.push(*r);
                if batch_rows.len() >= BATCH {
                    commit(&mut batch_rows, &mut batch_keys, &mut summary)?;
                    written += BATCH;
                    on_progress(ScanProgress::new("writing", written.min(total), total));
                }
            }
            Parsed::Failed(p) => {
                summary.failed += 1;
                seen.insert(p);
            }
        }
        parsed += 1;
        if parsed % 250 == 0 {
            on_progress(ScanProgress::new("parsing", parsed, total));
        }
    }
    commit(&mut batch_rows, &mut batch_keys, &mut summary)?;
    on_progress(ScanProgress::new("writing", total, total));

    // ---- 6) 清理已从磁盘移走的记录 ----
    //
    // 只在「属于本次某个扫描根目录、却又不在 seen 里」的记录上动手——
    // 没勾选的别的目录下的老照片不会被误删。
    on_progress(ScanProgress::new("stats", 0, 1));
    {
        let mut conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        let tx = conn.transaction()?;
        let mut stale: Vec<i64> = Vec::new();
        {
            let mut stmt = tx.prepare("SELECT id, path FROM photos")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let id: i64 = r.get(0)?;
                let path: String = r.get(1)?;
                let under_root = roots.iter().any(|r| Path::new(&path).starts_with(r));
                if under_root && !seen.contains(&path) {
                    stale.push(id);
                }
            }
        }
        if !stale.is_empty() {
            let mut del = tx.prepare("DELETE FROM photos WHERE id = ?1")?;
            for id in &stale {
                del.execute([id])?;
            }
            summary.removed = stale.len();
        }
        tx.commit()?;
    }

    // ---- 7) 配对统计 ----
    {
        let conn = db.lock().map_err(|e| anyhow!("{e}"))?;
        summarize(&conn, &mut summary)?;
    }

    summary.elapsed_ms = t0.elapsed().as_millis();
    Ok(summary)
}

/// 配对 / 孤立 / EXIF 失败的统计。抽出来是因为「空目录」提前返回的那条路径
/// 也要算一遍，避免 0 张时统计字段是 0 以外的脏值。
fn summarize(conn: &Connection, summary: &mut ScanSummary) -> Result<()> {
    {
        let mut stmt = conn.prepare(
            "SELECT
                COUNT(*),
                SUM(CASE WHEN cnt = 1 AND has_raw = 1 THEN 1 ELSE 0 END),
                SUM(CASE WHEN cnt = 1 AND has_jpg = 1 THEN 1 ELSE 0 END)
             FROM (
                SELECT pair_key,
                       COUNT(*) AS cnt,
                       MAX(CASE WHEN file_kind = 'raw'  THEN 1 ELSE 0 END) AS has_raw,
                       MAX(CASE WHEN file_kind = 'jpeg' THEN 1 ELSE 0 END) AS has_jpg
                FROM photos
                GROUP BY pair_key
             )",
        )?;
        let mut rows = stmt.query([])?;
        if let Some(r) = rows.next()? {
            summary.pairs = r.get::<_, i64>(0).unwrap_or(0) as usize;
            summary.orphan_raw =
                r.get::<_, Option<i64>>(1).unwrap_or(Some(0)).unwrap_or(0) as usize;
            summary.orphan_jpg =
                r.get::<_, Option<i64>>(2).unwrap_or(Some(0)).unwrap_or(0) as usize;
        }
    }

    {
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM photos WHERE exif_ok = 0")?;
        let mut rows = stmt.query([])?;
        if let Some(r) = rows.next()? {
            summary.exif_failed = r.get::<_, i64>(0).unwrap_or(0) as usize;
        }
    }
    Ok(())
}

/// 只把给定 pair 的主文件标记算对：RAW 优先，其次按 id。
///
/// 分批写库时每批只重算本批涉及的 pair，避免每次都全表 UPDATE。
fn set_primary_for(tx: &Transaction, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let holders = vec!["?"; keys.len()].join(",");
    let sql = format!(
        "UPDATE photos SET is_primary = (id IN (
            SELECT id FROM (
                SELECT id,
                       ROW_NUMBER() OVER (
                         PARTITION BY pair_key
                         ORDER BY (file_kind = 'raw') DESC, id
                       ) AS rn
                FROM photos WHERE pair_key IN ({holders})
            ) WHERE rn = 1))
         WHERE pair_key IN ({holders})"
    );
    let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(keys.len() * 2);
    for k in keys {
        params.push(rusqlite::types::Value::Text(k.clone()));
    }
    for k in keys {
        params.push(rusqlite::types::Value::Text(k.clone()));
    }
    tx.execute(&sql, rusqlite::params_from_iter(params))?;
    Ok(())
}

/// 文件夹范围选择时用到的一个节点。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirNode {
    /// 绝对路径
    pub path: String,
    /// 文件夹名（不含父级）
    pub name: String,
    /// 相对所选根目录的深度：根本身为 0，其直接子目录为 1，依此类推
    pub depth: usize,
    /// 是否还有更深的子目录
    pub has_children: bool,
}

/// 以广度优先列出 `root` 下的子目录（含 root 自身，depth=0），
/// 供「文件夹范围选择」弹窗做一棵可勾选的树。
///
/// 递归过深或目录爆炸时靠 `max_depth` / `max_nodes` 兜底，
/// 毕竟选片是给人挑照片用的，三千个子目录的树也没人看得过来。
pub fn list_subdirs(root: &Path, max_depth: usize, max_nodes: usize) -> Vec<DirNode> {
    let mut out: Vec<DirNode> = Vec::new();
    if !root.is_dir() {
        return out;
    }
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if out.len() >= max_nodes {
            break;
        }
        let mut kids: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                if let Ok(md) = e.metadata() {
                    if md.is_dir() {
                        kids.push(e.path());
                    }
                }
            }
        }
        kids.sort();

        if depth > 0 {
            out.push(DirNode {
                path: dir.to_string_lossy().to_string(),
                name: dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
                depth,
                has_children: !kids.is_empty(),
            });
        }
        if depth >= max_depth {
            continue;
        }
        for k in kids {
            queue.push_back((k, depth + 1));
        }
    }
    out
}

/// 单个文件的解析：先看「大小 + 修改时间」是否变过，没变就直接跳过；
/// 变了才算指纹、读 EXIF。
///
/// 这一条让增量扫描从「读几 GB」降到「纯 stat」——重扫一个没动过的图库
/// 是秒级的，所以应用启动时可以放心地自动重扫一遍。
fn parse_one(path: &Path, known: &HashMap<String, KnownFile>) -> Parsed {
    let path_str = path.to_string_lossy().to_string();

    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Parsed::Failed(path_str),
    };
    let file_size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // 快路径：大小与修改时间都没动过，内容几乎不可能变（改了内容又恰好
    // 保持同样的字节数、还把 mtime 改回来的情况，不在个人素材库里讨论）。
    if let Some(k) = known.get(&path_str) {
        if k.file_size == file_size && k.mtime == mtime {
            return Parsed::Unchanged(path_str);
        }
    }

    let fp = match fingerprint(path, file_size as u64, mtime) {
        Ok(f) => f,
        Err(_) => return Parsed::Failed(path_str),
    };

    if known
        .get(&path_str)
        .map(|k| k.fingerprint == fp)
        .unwrap_or(false)
    {
        return Parsed::Unchanged(path_str);
    }

    let kind = pairing::classify(&pairing::ext_lower(path)).unwrap_or(FileKind::Other);
    let pair_key = pairing::pair_key(path).unwrap_or_else(|| path_str.clone());

    let mut row = PhotoRow {
        path: path_str,
        fingerprint: fp,
        pair_key,
        file_kind: kind.as_str().to_string(),
        file_size,
        mtime,
        exif_ok: 0,
        ..Default::default()
    };

    if let Ok(ex) = read_exif(path) {
        row.taken_at = ex.taken_at;
        row.camera_model = ex.camera_model;
        row.camera_serial = ex.camera_serial;
        row.lens = ex.lens;
        row.focal_len = ex.focal_len;
        row.aperture = ex.aperture;
        row.shutter = ex.shutter;
        row.iso = ex.iso;
        row.orientation = ex.orientation;
        row.width = ex.width;
        row.height = ex.height;
        row.exif_ok = 1;
    }

    Parsed::Fresh(Box::new(row))
}

/// 快速指纹：文件大小 + 修改时间 + 前 1MB 内容哈希。
///
/// **刻意不做全文件哈希** —— 那会让首次导入从分钟级变成小时级。
/// 真正的全文件哈希只在「查找完全重复」的后台任务里按需计算。
fn fingerprint(path: &Path, size: u64, mtime: i64) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut head = vec![0u8; FINGERPRINT_HEAD_BYTES];
    let n = f.read(&mut head).unwrap_or(0);

    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    h.update(&mtime.to_le_bytes());
    h.update(&head[..n]);
    Ok(h.finalize().to_hex()[..32].to_string())
}

// ---------------------------------------------------------------------------
// EXIF
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ExifLite {
    taken_at: Option<i64>,
    camera_model: Option<String>,
    camera_serial: Option<String>,
    lens: Option<String>,
    focal_len: Option<f64>,
    aperture: Option<f64>,
    shutter: Option<String>,
    iso: Option<i64>,
    orientation: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}

fn read_exif(path: &Path) -> Result<ExifLite> {
    use exif::{In, Tag};

    let file = std::fs::File::open(path).with_context(|| format!("打开失败 {}", path.display()))?;
    let mut br = std::io::BufReader::new(file);
    let exif = exif::Reader::new()
        .read_from_container(&mut br)
        .with_context(|| format!("读取 EXIF 失败 {}", path.display()))?;

    let mut out = ExifLite::default();

    out.taken_at = exif
        .get_field(Tag::DateTimeOriginal, In::PRIMARY)
        .or_else(|| exif.get_field(Tag::DateTimeDigitized, In::PRIMARY))
        .and_then(ascii_raw)
        .and_then(|s| parse_exif_datetime(&s));

    out.camera_model = exif
        .get_field(Tag::Model, In::PRIMARY)
        .and_then(ascii_raw)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    out.camera_serial = exif
        .get_field(Tag::BodySerialNumber, In::PRIMARY)
        .and_then(ascii_raw)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    out.lens = exif
        .get_field(Tag::LensModel, In::PRIMARY)
        .and_then(ascii_raw)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    out.focal_len = exif
        .get_field(Tag::FocalLength, In::PRIMARY)
        .and_then(as_f64);

    out.aperture = exif.get_field(Tag::FNumber, In::PRIMARY).and_then(as_f64);

    out.shutter = exif.get_field(Tag::ExposureTime, In::PRIMARY).map(|f| {
        let secs = as_f64(f).unwrap_or(0.0);
        if secs > 0.0 && secs < 1.0 {
            let denom = (1.0 / secs).round() as i64;
            format!("1/{denom}")
        } else {
            format!("{secs}s")
        }
    });

    out.iso = exif
        .get_field(Tag::PhotographicSensitivity, In::PRIMARY)
        .and_then(as_u32)
        .map(|v| v as i64)
        .or_else(|| {
            exif.get_field(Tag::ISOSpeed, In::PRIMARY)
                .and_then(as_u32)
                .map(|v| v as i64)
        });

    out.orientation = exif
        .get_field(Tag::Orientation, In::PRIMARY)
        .and_then(as_u32)
        .map(|v| v as i64);

    // 像素尺寸优先取 EXIF 里的 ExifImageWidth/Height；缺失则留空，后续由缩略图阶段补
    out.width = exif
        .get_field(Tag::PixelXDimension, In::PRIMARY)
        .and_then(as_u32)
        .map(|v| v as i64);
    out.height = exif
        .get_field(Tag::PixelYDimension, In::PRIMARY)
        .and_then(as_u32)
        .map(|v| v as i64);

    Ok(out)
}

fn ascii_raw(f: &exif::Field) -> Option<String> {
    if let exif::Value::Ascii(ref v) = f.value {
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

fn as_f64(f: &exif::Field) -> Option<f64> {
    match f.value {
        exif::Value::Rational(ref v) => v.first().map(|r| r.to_f64()),
        exif::Value::SRational(ref v) => v.first().map(|r| r.to_f64()),
        exif::Value::Short(ref v) => v.first().map(|&x| x as f64),
        exif::Value::Long(ref v) => v.first().map(|&x| x as f64),
        _ => None,
    }
}

fn as_u32(f: &exif::Field) -> Option<u32> {
    f.value.get_uint(0)
}

/// 解析 EXIF 时间串 `2026:09:16 10:12:44` → epoch 秒。
fn parse_exif_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split(':');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: i64 = d.next()?.parse().ok()?;
    let da: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").parse().unwrap_or(0);
    if y < 1900 || !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    Some(civil_to_epoch(y, mo, da, hh, mi, ss))
}

/// 公历 → epoch 秒（Howard Hinnant 的 days_from_civil 算法，避免引入日期库）。
fn civil_to_epoch(y: i64, m: i64, d: i64, hh: i64, mi: i64, ss: i64) -> i64 {
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + hh * 3600 + mi * 60 + ss
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sp-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_exif_datetime() {
        // 2026-09-16 10:12:44 UTC
        assert_eq!(parse_exif_datetime("2026:09:16 10:12:44"), Some(1789553564));
    }

    #[test]
    fn rejects_garbage_datetime() {
        assert_eq!(parse_exif_datetime("0000:00:00 00:00:00"), None);
        assert_eq!(parse_exif_datetime("not a date"), None);
    }

    /// 端到端：两次快门（各一个 NEF + 一个 JPG）应被识别为 2 张照片、4 个文件。
    /// 同时验证增量扫描、清理已移走文件、孤立文件统计。
    #[test]
    fn scans_pairs_incrementally_and_reports_orphans() {
        let dir = temp_dir("pair");
        for i in ["0001", "0002"] {
            std::fs::write(dir.join(format!("DSC_{i}.NEF")), format!("nef-{i}")).unwrap();
            std::fs::write(dir.join(format!("DSC_{i}.JPG")), format!("jpg-{i}")).unwrap();
        }

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        // 首次扫描：4 个文件全部入库，配成 2 张照片
        let s = scan(&dir, &db).unwrap();
        assert_eq!(s.scanned, 4);
        assert_eq!(s.inserted, 4);
        assert_eq!(s.updated, 0);
        assert_eq!(s.pairs, 2, "两次快门应该配成 2 张照片，而不是 4 张");
        assert_eq!(s.orphan_raw, 0);
        assert_eq!(s.orphan_jpg, 0);

        // 再扫一次：指纹未变，应该全部跳过（增量扫描）
        let s2 = scan(&dir, &db).unwrap();
        assert_eq!(s2.unchanged, 4);
        assert_eq!(s2.inserted, 0);
        assert_eq!(s2.pairs, 2);

        // 删掉一个 JPG：扫描后应清理该记录，并报出一对孤立
        std::fs::remove_file(dir.join("DSC_0002.JPG")).unwrap();
        let s3 = scan(&dir, &db).unwrap();
        assert_eq!(s3.removed, 1);
        assert_eq!(s3.pairs, 2);
        assert_eq!(s3.orphan_raw, 1, "DSC_0002 只剩 NEF，应报缺 JPG");
        assert_eq!(s3.orphan_jpg, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一对 NEF + JPG 里，RAW 应该是主文件（缩略图与 1:1 对焦检查都取自它）。
    #[test]
    fn raw_is_primary_within_a_pair() {
        let dir = temp_dir("primary");
        // 故意让 JPG 的文件名排序靠前，验证主文件选择依据的是格式而不是路径
        std::fs::write(dir.join("DSC_0001.NEF"), "nef").unwrap();
        std::fs::write(dir.join("DSC_0001.JPG"), "jpg").unwrap();

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        scan(&dir, &db).unwrap();

        let pk = pairing::pair_key(&dir.join("DSC_0001.NEF")).unwrap();
        let kind: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT file_kind FROM photos WHERE pair_key = ?1 AND is_primary = 1",
                [&pk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "raw", "配对里主文件必须是 NEF");

        let primaries: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM photos WHERE pair_key = ?1 AND is_primary = 1",
                [&pk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(primaries, 1, "一对里只能有一个主文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 只拍了 JPG 的照片（没开 RAW）也要能正常入库，不能因为配不上就丢掉。
    #[test]
    fn jpeg_only_shots_are_kept() {
        let dir = temp_dir("jpgonly");
        std::fs::write(dir.join("IMG_9001.JPG"), "jpg-only").unwrap();

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        let s = scan(&dir, &db).unwrap();

        assert_eq!(s.scanned, 1);
        assert_eq!(s.pairs, 1);
        assert_eq!(s.orphan_jpg, 1, "只有 JPG 应报缺 NEF");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 增量扫描的快路径：内容没变过的文件，一个字节都不该被读。
    ///
    /// 验证手法：把库里存的指纹故意改错。
    /// - 若扫描走「读内容算指纹」，会因指纹不匹配而重新解析 → updated = 1
    /// - 若走「大小 + 修改时间」快路径，直接跳过 → unchanged = 1
    ///
    /// 这条性质决定了「每次启动自动重扫」是否可接受：快路径下是纯 stat，
    /// 几千张只需一两秒；退化成读内容就是几个 GB 的读取。
    #[test]
    fn unchanged_files_skip_content_read() {
        let dir = temp_dir("fastpath");
        std::fs::write(dir.join("DSC_0001.NEF"), "pretend-nef").unwrap();

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        scan(&dir, &db).unwrap();
        db.lock()
            .unwrap()
            .execute("UPDATE photos SET fingerprint = 'corrupted-on-purpose'", [])
            .unwrap();

        let s = scan(&dir, &db).unwrap();
        assert_eq!(s.unchanged, 1, "大小与修改时间都没变，不该重新读文件");
        assert_eq!(s.updated, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 进度必须按阶段推进，前端才能画出合理的进度条。
    #[test]
    fn reports_progress_through_phases() {
        let dir = temp_dir("progress");
        for i in 0..3 {
            std::fs::write(dir.join(format!("DSC_{i:04}.NEF")), format!("n{i}")).unwrap();
        }

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        let seen = std::sync::Mutex::new(Vec::new());
        scan_with_progress(
            &[dir.clone()],
            &db,
            |p| {
                seen.lock().unwrap().push(p.phase);
            },
            None,
        )
        .unwrap();

        let phases = seen.into_inner().unwrap();
        assert_eq!(phases.first().copied(), Some("walking"));
        assert!(phases.contains(&"parsing"), "应报告解析进度：{phases:?}");
        assert!(phases.contains(&"writing"), "应报告写入进度：{phases:?}");
        assert_eq!(phases.last().copied(), Some("stats"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件夹范围：只扫勾选的子目录，没勾选目录下的老照片不应被误删。
    #[test]
    fn scans_only_selected_subdirs() {
        let dir = temp_dir("scope");
        let a = dir.join("A");
        let b = dir.join("B");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("DSC_A.NEF"), "a").unwrap();
        std::fs::write(b.join("DSC_B.NEF"), "b").unwrap();

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));
        // 只扫 A 目录
        scan_with_progress(&[a.clone()], &db, |_| {}, None).unwrap();
        let only_a: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(only_a, 1, "只应扫到 A 里的 1 个文件");

        // 再扫 B：A 不应被清理（因为本次根目录不包含 A）
        scan_with_progress(&[b.clone()], &db, |_| {}, None).unwrap();
        let both: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(both, 2, "B 追加进来，A 的老记录应保留");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个子文件夹都不勾时：只读根目录自己那一层的照片，不进子文件夹。
    #[test]
    fn scans_only_the_root_level_when_depth_is_one() {
        let dir = temp_dir("shallow");
        let sub = dir.join("子文件夹");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.join("DSC_ROOT.NEF"), "root").unwrap();
        std::fs::write(sub.join("DSC_SUB.NEF"), "sub").unwrap();

        let db = Arc::new(Mutex::new(crate::db::open_in_memory().unwrap()));

        scan_with_progress(&[dir.clone()], &db, |_| {}, Some(1)).unwrap();
        let n: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "只应读到根目录自己的 1 个文件");

        // 不限层数时两个都该进来，确认上面的差别确实来自 max_depth
        scan_with_progress(&[dir.clone()], &db, |_| {}, None).unwrap();
        let all: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(all, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
