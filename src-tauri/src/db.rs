//! SQLite 连接与 schema。
//!
//! 三个关键设计（详见方案文档第八节）：
//! 1. `photos` 每个文件一行，用 `pair_key` 聚合配对 —— 兼容「只拍 JPG」的混合情况
//! 2. `decisions` 以 `pair_key` 为主键 —— 选片结果必须活过重建索引（见下方注释）
//! 3. `camera_bodies` 独立成表 —— 机身时间偏移是一次性配置、长期生效

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

/// 数据库文件名之外的附属文件（WAL / journal / shm）。
const SIDECARS: [&str; 3] = ["-journal", "-wal", "-shm"];

/// 打开数据库。失败时按「坏在哪儿」分三档处理，而不是一律重建：
///
/// 1. **残骸**（0 字节 / 写了一半 + 残留 journal）：里面不可能有数据，直接清掉重试。
///    最常见的现场是「上次在写入过程中被杀掉」，这一步能让启动少一次人工干预。
/// 2. **文件能读、只是表结构和代码对不上**：这是代码的问题，不是数据的问题。
///    隔离文件等于把用户的选片结果藏起来，所以宁可启动失败也一个字都不动。
/// 3. **文件真的坏了**（不是 SQLite、内容被覆盖）：改名隔离 + 重建。
///    挪而不是删：万一里面有东西。
pub fn open(path: &Path) -> Result<Connection> {
    let first = match open_inner(path) {
        Ok(conn) => return Ok(conn),
        Err(e) => e,
    };

    // 1) 残骸：里面不可能有用户数据，可以直接清掉
    let mut cleaned = false;
    if is_unusable_remnant(path) {
        if std::fs::remove_file(path).is_ok() {
            cleaned = true;
        }
    }
    if clear_sidecars(path) {
        cleaned = true;
    }

    if cleaned {
        if let Ok(conn) = open_inner(path) {
            return Ok(conn);
        }
    }

    // 2) 库文件本身是好的，只是 schema 应用不上。
    //    因为 `photos` 里的索引可以随时重建，而 `decisions` 里的选片结果是
    //    一晚上的手工成果、丢了就没了——所以这一档绝不能走隔离。
    if is_readable_sqlite(path) {
        return Err(first).context(format!(
            "数据库文件正常，但表结构没能应用（多半是新旧版本不兼容）。\n  \
             文件已原样保留、未做任何改动：{}\n  \
             这一档刻意不自动重建，以免丢掉已经做好的选片结果。",
            path.display()
        ));
    }

    // 3) 真的坏了：隔离 + 重建
    let backup = quarantine(path)
        .with_context(|| format!("数据库打开失败，且无法隔离损坏文件：{}", path.display()))?;
    match open_inner(path) {
        Ok(conn) => {
            eprintln!(
                "已隔离无法打开的库文件（备份为 {}），重新建库继续启动",
                backup.display()
            );
            Ok(conn)
        }
        Err(second) => Err(second).context(format!(
            "数据库无法打开：{}\n  第一次尝试：{first:#}\n  隔离文件：{}",
            path.display(),
            backup.display()
        )),
    }
}

/// 这个文件是不是一个「能读的 SQLite 库」（只看文件能不能读，不看表结构）。
///
/// 用来把「文件坏了」和「表结构和代码对不上」分开——这两者的正确处置完全相反：
/// 前者可以隔离重建，后者一旦隔离就等于把用户的数据藏了起来。
fn is_readable_sqlite(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match Connection::open(path) {
        // 打开是惰性的，必须真的读一下页头才能确认文件是 SQLite
        Ok(conn) => conn
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .is_ok(),
        Err(_) => false,
    }
}

/// 删掉 journal / wal / shm。返回是否真的删掉了东西。
fn clear_sidecars(path: &Path) -> bool {
    let mut removed = false;
    for suffix in SIDECARS {
        let side = std::path::PathBuf::from(format!("{}{}", path.display(), suffix));
        if side.exists() && std::fs::remove_file(&side).is_ok() {
            removed = true;
        }
    }
    removed
}

/// 库文件是不是「写入中断留下的残骸」。
///
/// 判据是「小于一个页（512 字节）」——SQLite 最小的有效数据库文件正好是一页，
/// 比它更小的文件不可能是数据库，也就不可能装着用户数据，可以直接丢掉。
///
/// 超过这个大小的文件一律走「改名隔离」：头部看着不对也可能是别的原因，
/// 里面也许有用户辛苦打的分，宁可多留一个 .bad 文件也不冒删错的风险。
fn is_unusable_remnant(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(md) => md.len() < 512,
        Err(_) => false,
    }
}

fn open_inner(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // 扫描是后台写入 + 前端同时读，锁冲突时等一下而不是立刻报错
    conn.busy_timeout(Duration::from_secs(5))?;

    // WAL 让读写不互相阻塞，是首选；但它依赖文件系统支持额外的 shm 文件，
    // 某些环境（网络盘、受限沙箱）拿不到。拿不到就退回默认日志模式，
    // 只是性能略差，不该因此启动失败。
    if let Err(e) = conn.pragma_update(None, "journal_mode", "WAL") {
        eprintln!("提示：WAL 模式不可用（{e}），改用默认日志模式");
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)?;
    drop_empty_legacy_table(&conn);
    Ok(conn)
}

/// 一次性清理：早期 schema 里有一张 `ratings` 占位表，但从来没有代码往里写过。
/// 现在 `decisions` 取代了它 —— 留两张语义重复的表，只会让下一个读代码的人
/// 以为「评分在这里、标记在那里」。只在确认它确实是空的时才删。
fn drop_empty_legacy_table(conn: &Connection) {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ratings'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return;
    }

    // 数不出来就当作「有东西」，宁可留着一张废表也不冒删错的风险
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM ratings", [], |r| r.get(0))
        .unwrap_or(i64::MAX);
    if rows == 0 {
        let _ = conn.execute_batch("DROP TABLE IF EXISTS ratings");
    } else {
        eprintln!("提示：库里有一张旧表 ratings（{rows} 行），保留未动");
    }
}

/// 把库文件连同 journal/wal/shm 一起改名挪走，返回备份主文件名。
fn quarantine(path: &Path) -> Result<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = path.to_path_buf();
    let file_name = backup
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "library.db".to_string());
    backup.set_file_name(format!("{file_name}.bad-{stamp}"));

    if path.exists() {
        std::fs::rename(path, &backup).with_context(|| format!("无法重命名 {}", path.display()))?;
    }
    clear_sidecars(path);
    Ok(backup)
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS photos (
  id                 INTEGER PRIMARY KEY,
  path               TEXT NOT NULL UNIQUE,
  fingerprint        TEXT NOT NULL DEFAULT '',
  content_hash       TEXT,
  phash              TEXT,
  pair_key           TEXT NOT NULL DEFAULT '',
  file_kind          TEXT NOT NULL DEFAULT 'other',
  is_primary         INTEGER NOT NULL DEFAULT 1,
  decode_path        TEXT,
  file_size          INTEGER NOT NULL DEFAULT 0,
  mtime              INTEGER NOT NULL DEFAULT 0,
  width              INTEGER,
  height             INTEGER,
  taken_at           INTEGER,
  taken_at_corrected INTEGER,
  camera_model       TEXT,
  camera_serial      TEXT,
  lens               TEXT,
  focal_len          REAL,
  aperture           REAL,
  shutter            TEXT,
  iso                INTEGER,
  orientation        INTEGER,
  exif_ok            INTEGER NOT NULL DEFAULT 0,
  indexed_at         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_photos_pair  ON photos(pair_key);
CREATE INDEX IF NOT EXISTS idx_photos_time  ON photos(taken_at_corrected);
CREATE INDEX IF NOT EXISTS idx_photos_kind  ON photos(file_kind);

CREATE TABLE IF NOT EXISTS camera_bodies (
  serial              TEXT PRIMARY KEY,
  model               TEXT,
  time_offset_seconds INTEGER NOT NULL DEFAULT 0,
  first_seen_at       INTEGER,
  note                TEXT
);

-- 选片结果（保留 / 淘汰 + 星级）。
--
-- **主键必须是 pair_key，不能是 photo_id。** 这是整张表唯一重要的决定：
-- 重新扫描时 photos 会被删掉重建（移走的文件消失、留下的文件拿到新的自增 id），
-- 任何挂在 photo_id 上的外键都会被 ON DELETE CASCADE 连带清空 ——
-- 也就是说「重扫一次，辛苦打了一晚上的分全没了」。
-- pair_key 由路径推导，只要文件还在原地就不变，标记才能长期存活。
CREATE TABLE IF NOT EXISTS decisions (
  pair_key   TEXT PRIMARY KEY,
  decision   TEXT NOT NULL DEFAULT 'none',   -- none / keep / reject
  stars      INTEGER NOT NULL DEFAULT 0,     -- 0–5
  updated_at INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_decisions_decision ON decisions(decision);

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

/// 内存库。测试用；同时是「库文件打不开」时让应用还能起来的兜底。
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// 读取一个 meta 值。
#[allow(dead_code)]
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
    let mut rows = stmt.query([key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

/// 写入一个 meta 值。
#[allow(dead_code)]
pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 造一个干净的临时库路径（含 journal / wal / shm 残留的清理）。
    fn temp_db(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("sp-test-{}-{}.db", name, std::process::id()));
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", p.display(), suffix));
        }
        p
    }

    fn cleanup(path: &PathBuf) {
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    /// 之前的测试全走内存库，`open()` 的文件分支从没被覆盖过——
    /// 而这个分支正是启动时 panic 的地方。
    #[test]
    fn opens_file_db_with_wal() {
        let path = temp_db("open");
        let conn = open(&path).expect("文件库应该能打开");

        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");

        conn.execute("INSERT INTO meta(key, value) VALUES('k','v')", [])
            .unwrap();
        assert_eq!(meta_get(&conn, "k").unwrap().as_deref(), Some("v"));

        drop(conn);
        cleanup(&path);
    }

    /// 复现启动现场：库文件存在但为空、外加一个残留的 journal。
    /// 这种状态不该让启动崩掉。
    #[test]
    fn recovers_from_stale_journal() {
        let path = temp_db("stale");
        std::fs::write(&path, b"").unwrap();
        std::fs::write(format!("{}-journal", path.display()), [0u8; 512]).unwrap();

        let conn = open(&path).expect("残留 journal 不该阻断启动");
        conn.execute("INSERT INTO meta(key, value) VALUES('k','v')", [])
            .unwrap();
        assert_eq!(meta_get(&conn, "k").unwrap().as_deref(), Some("v"));

        drop(conn);
        cleanup(&path);
    }

    /// 打开两次（模拟重开应用）不该出问题。
    #[test]
    fn reopens_existing_db() {
        let path = temp_db("reopen");
        {
            let conn = open(&path).unwrap();
            meta_set(&conn, "last_scan", "123").unwrap();
        }
        let conn = open(&path).unwrap();
        assert_eq!(
            meta_get(&conn, "last_scan").unwrap().as_deref(),
            Some("123")
        );
        drop(conn);
        cleanup(&path);
    }

    /// 库文件内容不是数据库（比如被别的东西覆盖了）时，
    /// 应该隔离坏文件、重建库、正常启动，而不是崩在启动阶段。
    #[test]
    fn quarantines_corrupt_file_instead_of_failing() {
        let path = temp_db("corrupt");
        // 大小要超过一页，才走「改名隔离」而不是「当残骸丢掉」
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();

        let conn = open(&path).expect("坏库文件应该被隔离后重建");
        conn.execute("INSERT INTO meta(key, value) VALUES('k','v')", [])
            .unwrap();
        assert_eq!(meta_get(&conn, "k").unwrap().as_deref(), Some("v"));
        drop(conn);

        // 坏文件应该还在（改名备份），没被删掉
        let dir = path.parent().unwrap();
        let prefix = format!("{}.bad-", path.file_name().unwrap().to_string_lossy());
        let backups: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        assert_eq!(backups.len(), 1, "应该留下且只留下一个备份文件");

        cleanup(&path);
        for name in backups {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }

    /// 「写入中断留下的残骸」判定：小于一页。
    /// 这一条守着「什么该直接删、什么必须留着」这条界线。
    #[test]
    fn treats_only_sub_page_files_as_remnants() {
        let path = temp_db("remnant");

        std::fs::write(&path, b"").unwrap();
        assert!(is_unusable_remnant(&path), "空文件是残骸");

        std::fs::write(&path, vec![b'x'; 511]).unwrap();
        assert!(is_unusable_remnant(&path), "写了一半的文件是残骸");

        std::fs::write(&path, vec![b'x'; 512]).unwrap();
        assert!(
            !is_unusable_remnant(&path),
            "够一页就必须保留，宁可改名隔离"
        );

        // 文件不存在时不是残骸（不该去删一个不存在的路径）
        cleanup(&path);
        assert!(!is_unusable_remnant(&path));
    }

    /// 复现本次实际遇到的现场：上次在首次写入过程中被杀掉，
    /// 留下「0 字节库 + 热 journal」。这种状态必须自愈，
    /// 而不是每次都失败到启动不了。
    #[test]
    fn recovers_from_empty_db_left_by_interrupted_write() {
        let path = temp_db("hotjournal");
        std::fs::write(&path, b"").unwrap();
        // 一个看起来像那么回事的 journal 头
        let mut junk = vec![0u8; 512];
        junk[..4].copy_from_slice(&[0xd9, 0xd5, 0x05, 0xf9]);
        std::fs::write(format!("{}-journal", path.display()), junk).unwrap();

        let conn = open(&path).expect("残骸应被清掉后重建，而不是卡住");
        meta_set(&conn, "k", "v").unwrap();
        assert_eq!(meta_get(&conn, "k").unwrap().as_deref(), Some("v"));
        drop(conn);

        // 这次是在原地重建的，不该留下 .bad 备份
        let dir = path.parent().unwrap();
        let prefix = format!("{}.bad-", path.file_name().unwrap().to_string_lossy());
        let backups = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count();
        assert_eq!(backups, 0, "残骸不该产生备份文件");

        cleanup(&path);
    }

    /// 选片结果表里的主键必须是 pair_key。
    ///
    /// 这条不是「记录现状」，而是守着上面那条注释里写的理由：
    /// 一旦有人把主键改回 photo_id 并加上外键，重扫一次用户的标记就会全没。
    /// 真出了那种事，是在用户身上才发现的。
    #[test]
    fn decisions_are_keyed_by_pair_not_by_photo_id() {
        let conn = open_in_memory().unwrap();

        let (name, pk): (String, i64) = conn
            .query_row(
                "SELECT name, pk FROM pragma_table_info('decisions') WHERE name = 'pair_key'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("decisions 表必须有 pair_key 列");
        assert_eq!(name, "pair_key");
        assert_eq!(pk, 1, "pair_key 必须排在主键第一位");

        // decisions 不该引用 photos —— 外键会跟着重扫的删除一起把标记带走
        let refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list('decisions')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refs, 0, "decisions 不能有外键指向 photos");
    }

    /// 从老版本升级：库里还留着 `ratings` 那张占位表。
    /// 打开一次之后，新表要在、废表要走掉，而且原有数据一个不少。
    ///
    /// 老库和新库的差别**只有这一张表**（其它表的定义一直是 `IF NOT EXISTS`），
    /// 所以这里用「先建成新库、再把 decisions 换成 ratings」来还原升级现场——
    /// 比手抄一份完整的老 schema 更不容易抄错。
    #[test]
    fn upgrades_an_existing_library_in_place() {
        let path = temp_db("upgrade");

        {
            let conn = open(&path).unwrap();
            meta_set(&conn, "旧", "别弄丢我").unwrap();
            conn.execute(
                "INSERT INTO photos(path, pair_key, file_kind) VALUES ('/x/DSC_0001.NEF', '/x/dsc_0001', 'raw')",
                [],
            )
            .unwrap();
            // 还原成老版本的样子：有 ratings、没有 decisions
            conn.execute_batch(
                "DROP TABLE decisions;
                 CREATE TABLE ratings (
                   photo_id INTEGER PRIMARY KEY REFERENCES photos(id) ON DELETE CASCADE,
                   pair_key TEXT NOT NULL DEFAULT '',
                   stars INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .unwrap();
        }

        let conn = open(&path).expect("老库应当能原地升级");
        let table_exists = |name: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                [name],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(table_exists("decisions"), 1, "新表要建出来");
        assert_eq!(table_exists("ratings"), 0, "空的旧表要清掉");
        assert_eq!(
            meta_get(&conn, "旧").unwrap().as_deref(),
            Some("别弄丢我"),
            "升级不能动老数据"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM photos", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "升级不能动照片索引"
        );

        drop(conn);
        cleanup(&path);
    }

    /// 表结构和代码对不上时**不能隔离文件**。
    ///
    /// 以前这一档会走「改名隔离 + 重建」，结果是：新版本的表结构一旦和旧库不一致，
    /// 用户辛苦打了一晚上的选片结果就被悄悄挪进一个 `.bad-` 文件里、
    /// 界面上看到的是一间空屋子。照片索引可以重建，选片结果不能。
    #[test]
    fn refuses_to_quarantine_on_schema_mismatch() {
        let path = temp_db("drift");

        // 一个结构不对但确实是 SQLite 的库
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE photos (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE);
                 INSERT INTO photos(path) VALUES ('/x/DSC_0001.NEF');",
            )
            .unwrap();
        }
        let size_before = std::fs::metadata(&path).unwrap().len();

        let err = open(&path).expect_err("结构对不上时应当明确报错，而不是偷偷重建");
        let msg = format!("{err:#}");
        assert!(msg.contains("表结构"), "报错要说清是哪一类问题：{msg}");

        assert!(path.exists(), "原文件必须原地不动");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            size_before,
            "原文件不该被改动"
        );

        // 也不该留下任何 .bad- 备份（那会让人以为数据还在别处）
        let dir = path.parent().unwrap();
        let prefix = format!("{}.bad-", path.file_name().unwrap().to_string_lossy());
        let backups = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count();
        assert_eq!(backups, 0, "不该产生隔离备份");

        // 原始数据还在（这里用能读出来证明）
        assert!(is_readable_sqlite(&path));
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM photos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "数据一个字都没少");

        drop(conn);
        cleanup(&path);
    }

    /// 旧的 `ratings` 占位表：空的就删掉，有东西就留着。
    #[test]
    fn drops_only_the_empty_legacy_table() {
        let conn = open_in_memory().unwrap();
        let exists = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ratings'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        };

        // 表不存在时调用不该出问题
        drop_empty_legacy_table(&conn);
        assert_eq!(exists(&conn), 0);

        // 空表 → 删掉
        conn.execute_batch("CREATE TABLE ratings (photo_id INTEGER PRIMARY KEY)")
            .unwrap();
        drop_empty_legacy_table(&conn);
        assert_eq!(exists(&conn), 0, "空的旧表应当被清掉");

        // 有数据 → 留着：宁可多一张废表，也不冒删错的风险
        conn.execute_batch("CREATE TABLE ratings (photo_id INTEGER PRIMARY KEY)")
            .unwrap();
        conn.execute("INSERT INTO ratings(photo_id) VALUES(1)", [])
            .unwrap();
        drop_empty_legacy_table(&conn);
        assert_eq!(exists(&conn), 1, "有内容的旧表必须留下");
    }
}
