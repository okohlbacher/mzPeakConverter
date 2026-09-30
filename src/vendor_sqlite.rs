//! Opening a vendor SQLite database (Bruker `analysis.tsf` / `.tdf`, the BAF `analysis.sqlite`
//! cache, HyStar traces) without writing anything into the user's raw data folder.
//!
//! `SQLITE_OPEN_READ_ONLY` is not enough: on a database in WAL mode a read-only connection creates
//! `<db>-shm` and `<db>-wal` beside it and, unable to checkpoint, leaves them there — which is what a
//! Bruker MALDI TSF conversion did to the issue author's `.d` (HUPO-PSI/mzPeak-specification#23).
//! `immutable=1` tells SQLite the file cannot change, so it creates nothing and takes no locks.
//!
//! The catch: an immutable open reads the database file alone. It ignores the WAL, and a `-wal` left
//! non-empty by the acquisition software holds committed rows; and it skips the hot-journal check,
//! so the rollback `-journal` of a crashed or still-writing acquisition is not rolled back and a
//! half-written database is read as if it were consistent, where a plain read-only open refused
//! (review 2026-09-30). Neither can be read in place without side files — a read-only open creates
//! the `-shm` when there is none and never deletes it, and rolling a journal back writes the
//! database — so the database and that file are copied into a private scratch directory, read from
//! there into memory, and the scratch directory is removed ([`open_copy`]).
//!
//! Not covered: baf2sql, the Bruker library the BAF lane reads through, creates its `analysis.sqlite`
//! cache beside `analysis.baf`, inside the `.d`, when the run has none (`bruker_baf.rs`; review
//! 2026-09-30). This module then opens that cache like any other database, but the file itself is
//! the library's doing, so a BAF conversion can leave it in the raw folder.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rusqlite::{Connection, OpenFlags};

/// Open `path` read-only, creating no side file (see the module docs).
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if has_wal_content(path) {
        log::warn!(
            "{} has a non-empty -wal beside it (committed rows the acquisition software never \
             checkpointed); reading a copy of both through the WAL",
            path.display()
        );
        return open_copy(path, "-wal");
    }
    if has_hot_journal(path) {
        log::warn!(
            "{} has a hot rollback journal beside it (a transaction the acquisition software never \
             finished); reading a copy of both, rolled back",
            path.display()
        );
        return open_copy(path, "-journal");
    }
    Connection::open_with_flags(immutable_uri(path), flags | OpenFlags::SQLITE_OPEN_URI)
}

/// `<path><suffix>`: the `-wal` / `-journal` SQLite keeps beside a database.
fn side(path: &Path, suffix: &str) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(suffix);
    p.into()
}

fn has_wal_content(path: &Path) -> bool {
    std::fs::metadata(side(path, "-wal")).is_ok_and(|m| m.len() > 0)
}

/// The rollback journal's header magic. SQLite writes it before it overwrites the first database
/// page, and a journal without it restores nothing: one truncated to nothing (`journal_mode=
/// TRUNCATE`, the 32 zero-byte HyStar journals in the corpus) or with its header zeroed (`PERSIST`)
/// belongs to a finished transaction, and the database file alone is consistent.
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];

/// Whether `<path>-journal` is hot: it starts with [`JOURNAL_MAGIC`].
fn has_hot_journal(path: &Path) -> bool {
    let mut magic = [0u8; 8];
    std::fs::File::open(side(path, "-journal")).and_then(|mut f| f.read_exact(&mut magic)).is_ok()
        && magic == JOURNAL_MAGIC
}

/// A database whose `-wal` holds committed rows or whose `-journal` is hot: the database and that
/// file are copied into a private scratch directory and the copy is opened read-write, so SQLite
/// reads through the WAL (writing its `-shm` there) or rolls the journal back there; it is backed
/// up into an in-memory database and the scratch directory is removed. The in-memory copy is
/// query-only. ponytail: the whole database in memory, and a live writer can change the files
/// between the two copies — only for a WAL or journal left behind by a crashed or running
/// acquisition.
///
/// The copies are new files, not `fs::copy`s: those keep the source's permissions, and a raw
/// folder made read-only to protect it gave a read-only copy, which SQLite opens read-only in
/// silence and then cannot roll back (`SQLITE_READONLY_ROLLBACK`; review 2026-09-30 verification).
fn open_copy(path: &Path, suffix: &str) -> rusqlite::Result<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let scratch = std::env::temp_dir().join(format!("mzpc-sqlite-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let io = |e: std::io::Error| {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN), Some(format!("copying {} with its {suffix}: {e}", path.display())))
    };
    let copy_file = |from: &Path, to: &Path| std::io::copy(&mut std::fs::File::open(from)?, &mut std::fs::File::create(to)?);
    let read = || -> rusqlite::Result<Connection> {
        std::fs::create_dir_all(&scratch).map_err(io)?;
        let copy = scratch.join("db");
        copy_file(path, &copy).map_err(io)?;
        copy_file(&side(path, suffix), &side(&copy, suffix)).map_err(io)?;
        let src = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        // The first read rolls the journal back; a failure there carries SQLite's message, which
        // the backup below would report as the in-memory database's "not an error".
        src.query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))?;
        let mut mem = Connection::open_in_memory()?;
        rusqlite::backup::Backup::new(&src, &mut mem)?.run_to_completion(4096, std::time::Duration::ZERO, None)?;
        mem.pragma_update(None, "query_only", true)?;
        Ok(mem)
    };
    let out = read();
    let _ = std::fs::remove_dir_all(&scratch);
    out
}

/// `file:<path>?immutable=1`, every byte outside the URI-safe set percent-encoded — SQLite decodes
/// `%HH` back to the byte, so a Unix name that is not UTF-8 or holds a `\` reaches the file system
/// as it is (review 2026-09-30: `to_string_lossy` and a blanket `\` → `/` opened another file, or
/// none). Only a Windows path turns `\` into `/`: `C:\…` becomes `file:///C:/…` and
/// `\\server\share\…` becomes `file:////server/share/…`, the forms SQLite expects.
fn immutable_uri(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(abs.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = abs.to_string_lossy().replace('\\', "/").into_bytes();
    let mut uri = String::from(if bytes.starts_with(b"/") { "file://" } else { "file:///" });
    for &b in &bytes {
        if b.is_ascii_alphanumeric() || b"/:-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push_str("?immutable=1");
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    fn side_files(db: &Path) -> Vec<String> {
        ["-wal", "-shm", "-journal"]
            .iter()
            .filter(|s| {
                let mut p = db.as_os_str().to_owned();
                p.push(s);
                Path::new(&p).exists()
            })
            .map(|s| s.to_string())
            .collect()
    }

    /// A fresh scratch directory whose name needs escaping in a URI.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mzpc-vsql {tag} #1 %{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Every file in `dir` with its bytes, to show an open left it exactly as found.
    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        let mut v: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .map(|e| (e.file_name().to_string_lossy().into_owned(), std::fs::read(e.path()).unwrap()))
            .collect();
        v.sort();
        v
    }

    /// A WAL-mode database like the MALDI `analysis.tsf` in the report.
    fn wal_db(tag: &str) -> PathBuf {
        let db = scratch_dir(tag).join("analysis.tsf");
        let c = Connection::open(&db).unwrap();
        let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal");
        c.execute_batch("CREATE TABLE Frames (Id INTEGER); INSERT INTO Frames VALUES (1), (2), (3);").unwrap();
        drop(c);
        assert!(side_files(&db).is_empty(), "a clean close checkpoints and removes the WAL");
        db
    }

    #[test]
    fn a_wal_database_is_read_without_creating_side_files() {
        let db = wal_db("clean");
        // The mechanism: an ordinary read-only open leaves the WAL files behind.
        {
            let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let n: i64 = c.query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 3);
        }
        let leaked = side_files(&db);
        assert!(!leaked.is_empty(), "expected SQLITE_OPEN_READ_ONLY to leave -shm/-wal on a WAL database");
        for s in &leaked {
            let mut p = db.as_os_str().to_owned();
            p.push(s);
            std::fs::remove_file(&p).unwrap();
        }
        // The fix.
        {
            let c = open(&db).unwrap();
            let n: i64 = c.query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 3);
        }
        assert_eq!(side_files(&db), Vec::<String>::new(), "vendor_sqlite::open wrote into the input folder");
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// Rows still in a non-empty WAL (the acquisition software's connection is open) are read, not
    /// silently skipped by the immutable open.
    #[test]
    fn committed_rows_still_in_the_wal_are_read() {
        let db = wal_db("hot");
        let writer = Connection::open(&db).unwrap();
        writer.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO Frames VALUES (4), (5);").unwrap();
        let c = open(&db).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5, "the two frames in the WAL must be seen");
        assert!(c.execute("INSERT INTO Frames VALUES (6)", []).is_err(), "the copy is query-only");
        drop(c);
        drop(writer);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }

    /// A `.d` copied while the acquisition wrote: a non-empty `-wal` and no `-shm`. A read-only open
    /// in place would create the `-shm` beside the data and leave it there (reproduced 2026-09-30);
    /// the rows are read and the folder is left exactly as found.
    #[test]
    fn a_wal_without_its_shm_is_read_without_creating_one() {
        let db = wal_db("noshm");
        let dir = db.parent().unwrap().to_path_buf();
        let copy_dir = dir.join("copied.d");
        std::fs::create_dir_all(&copy_dir).unwrap();
        let copy = copy_dir.join("analysis.tsf");
        {
            let writer = Connection::open(&db).unwrap();
            writer.execute_batch("PRAGMA wal_autocheckpoint=0; INSERT INTO Frames VALUES (4), (5);").unwrap();
            std::fs::copy(&db, &copy).unwrap();
            std::fs::copy(side(&db, "-wal"), side(&copy, "-wal")).unwrap();
        }
        let before = snapshot(&copy_dir);
        assert_eq!(before.len(), 2, "{:?}", before.iter().map(|(n, _)| n).collect::<Vec<_>>());
        let c = open(&copy).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5);
        drop(c);
        assert!(snapshot(&copy_dir) == before, "the open wrote into the folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.d` copied while the acquisition software was inside a transaction, in rollback-journal
    /// mode like every HyStar database: the transaction's pages already spilled into the database
    /// file and its `-journal` holds the pages they replaced. An immutable open reads the half-written
    /// file (review 2026-09-30); the copy rolled back is read instead, and the folder is left exactly
    /// as found.
    #[test]
    fn a_hot_journal_is_rolled_back_in_a_copy() {
        let dir = scratch_dir("journal");
        let db = dir.join("chromatography-data.sqlite");
        let copy_dir = dir.join("copied.d");
        std::fs::create_dir_all(&copy_dir).unwrap();
        let copy = copy_dir.join("chromatography-data.sqlite");
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "PRAGMA journal_mode=DELETE;
                 CREATE TABLE TraceChunks (Trace INTEGER, Times BLOB);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
                     INSERT INTO TraceChunks SELECT 1, zeroblob(100) FROM n;",
            )
            .unwrap();
        {
            // A fresh connection with a one-page cache, so the update spills: a warm cache can hold
            // every page, since the bundled SQLite shares its page cache limit between connections.
            let writer = Connection::open(&db).unwrap();
            writer.execute_batch("PRAGMA cache_size=1; BEGIN; UPDATE TraceChunks SET Trace = 2;").unwrap();
            std::fs::copy(&db, &copy).unwrap();
            std::fs::copy(side(&db, "-journal"), side(&copy, "-journal")).unwrap();
        }
        assert!(has_hot_journal(&copy), "the spill must have written the journal header");
        let count = |c: &Connection, sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        // The mechanism: the database file alone holds uncommitted rows.
        let immutable = Connection::open_with_flags(immutable_uri(&copy), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI).unwrap();
        assert!(count(&immutable, "SELECT count(*) FROM TraceChunks WHERE Trace = 2") > 0, "no uncommitted page reached the file");
        drop(immutable);
        // The fix, on a folder made read-only to protect it: the scratch copies must still be
        // writable for the rollback.
        for f in [copy.clone(), side(&copy, "-journal")] {
            let mut p = std::fs::metadata(&f).unwrap().permissions();
            p.set_readonly(true);
            std::fs::set_permissions(&f, p).unwrap();
        }
        let before = snapshot(&copy_dir);
        let c = open(&copy).unwrap();
        assert_eq!(count(&c, "SELECT count(*) FROM TraceChunks WHERE Trace = 1"), 2000, "the database as last committed");
        drop(c);
        assert!(snapshot(&copy_dir) == before, "the open wrote into the folder");
        // A journal that restores nothing is not hot: zero bytes (TRUNCATE), a zeroed header (PERSIST).
        std::fs::write(side(&db, "-journal"), b"").unwrap();
        assert!(!has_hot_journal(&db));
        std::fs::write(side(&db, "-journal"), [0u8; 512]).unwrap();
        assert!(!has_hot_journal(&db));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn uri_escapes_and_makes_relative_paths_absolute() {
        assert_eq!(immutable_uri(Path::new("/a b/c#1?%.d/x.tdf")), "file:///a%20b/c%231%3F%25.d/x.tdf?immutable=1");
        let rel = immutable_uri(Path::new("run.d/analysis.tsf"));
        assert!(rel.starts_with("file:///") && rel.ends_with("/run.d/analysis.tsf?immutable=1"), "{rel}");
        // A Unix name's bytes, as they are: a `\` is not a separator, and a name need not be UTF-8.
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(immutable_uri(Path::new(r"/a\b.d/x.tdf")), "file:///a%5Cb.d/x.tdf?immutable=1");
        assert_eq!(immutable_uri(Path::new(std::ffi::OsStr::from_bytes(b"/caf\xe9.d/x.tdf"))), "file:///caf%E9.d/x.tdf?immutable=1");
    }

    /// The file opened is the one named, when its Unix name holds a `\` (a blanket `\` → `/` opened
    /// `run/1.d/…` for `run\1.d/…`) or is not UTF-8 (`to_string_lossy` named a file that does not
    /// exist); review 2026-09-30.
    #[test]
    #[cfg(unix)]
    fn a_unix_name_with_a_backslash_or_not_utf8_opens_that_file() {
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch_dir("names");
        let create = |db: &Path, v: i64| {
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            Connection::open(db).unwrap().execute_batch(&format!("CREATE TABLE T (v); INSERT INTO T VALUES ({v});")).unwrap();
        };
        let value = |db: &Path| open(db).unwrap().query_row("SELECT v FROM T", [], |r| r.get::<_, i64>(0)).unwrap();
        let backslash = dir.join(r"run\1.d").join("analysis.tsf");
        create(&backslash, 1);
        create(&dir.join("run").join("1.d").join("analysis.tsf"), 2);
        assert_eq!(value(&backslash), 1);
        let latin1 = dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.d")).join("analysis.tsf");
        // ponytail: APFS and HFS+ refuse a name that is not UTF-8, so on macOS only the URI test
        // above covers it; Linux (CI) opens the file.
        if std::fs::create_dir_all(latin1.parent().unwrap()).is_ok() {
            create(&latin1, 3);
            assert_eq!(value(&latin1), 3);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(windows)]
    fn uri_for_a_windows_path() {
        assert_eq!(immutable_uri(Path::new(r"C:\Users\u\run d\analysis.tsf")), "file:///C:/Users/u/run%20d/analysis.tsf?immutable=1");
    }

    /// A `.d` on a network share: `\\server\share\…` becomes `file:////server/share/…`, which SQLite
    /// hands to Windows as `//server/share/…` (PR #35's `file://server/share/…` names a host SQLite
    /// rejects). Opened for real through the administrative share `\\localhost\C$` when it is there.
    #[test]
    #[cfg(windows)]
    fn a_unc_path_opens() {
        assert_eq!(immutable_uri(Path::new(r"\\server\share\run d\analysis.tsf")), "file:////server/share/run%20d/analysis.tsf?immutable=1");
        let db = std::path::absolute(wal_db("unc")).unwrap();
        let local = db.to_str().unwrap();
        let unc = PathBuf::from(format!(r"\\localhost\{}${}", &local[..1], &local[2..]));
        if std::fs::metadata(&unc).is_ok() {
            let n: i64 = open(&unc).unwrap().query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 3, "{}", unc.display());
        }
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
    }
}
