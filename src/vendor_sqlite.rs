//! Opening a vendor SQLite database (Bruker `analysis.tsf` / `.tdf` / `.baf` caches, HyStar traces)
//! without writing anything into the user's raw data folder.
//!
//! `SQLITE_OPEN_READ_ONLY` is not enough: on a database in WAL mode a read-only connection creates
//! `<db>-shm` and `<db>-wal` beside it and, unable to checkpoint, leaves them there — which is what a
//! Bruker MALDI TSF conversion did to the issue author's `.d` (HUPO-PSI/mzPeak-specification#23).
//! `immutable=1` tells SQLite the file cannot change, so it creates nothing and takes no locks.
//!
//! The catch: an immutable open also ignores the WAL, and a `-wal` left non-empty by the acquisition
//! software holds committed rows. Reading those in place is not possible without side files — a
//! read-only open creates the `-shm` when there is none, and never deletes it (review 2026-09-30) —
//! so the database and its WAL are copied into a private scratch directory, read from there into
//! memory, and the scratch directory is removed ([`open_through_wal`]).

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
        return open_through_wal(path);
    }
    Connection::open_with_flags(immutable_uri(path), flags | OpenFlags::SQLITE_OPEN_URI)
}

fn wal_of(path: &Path) -> PathBuf {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    wal.into()
}

fn has_wal_content(path: &Path) -> bool {
    std::fs::metadata(wal_of(path)).is_ok_and(|m| m.len() > 0)
}

/// A database whose `-wal` holds committed rows: the database and its WAL are copied into a private
/// scratch directory, read from the copy (SQLite writes its `-shm` there) into an in-memory
/// database, and the scratch directory is removed. The in-memory copy is query-only. ponytail: the
/// whole database in memory — only for a WAL left behind by a crashed or running acquisition.
fn open_through_wal(path: &Path) -> rusqlite::Result<Connection> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let scratch = std::env::temp_dir().join(format!("mzpc-wal-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let io = |e: std::io::Error| {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN), Some(format!("copying {} to read its WAL: {e}", path.display())))
    };
    let read = || -> rusqlite::Result<Connection> {
        std::fs::create_dir_all(&scratch).map_err(io)?;
        let copy = scratch.join("db");
        std::fs::copy(path, &copy).map_err(io)?;
        std::fs::copy(wal_of(path), wal_of(&copy)).map_err(io)?;
        let src = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        let mut mem = Connection::open_in_memory()?;
        rusqlite::backup::Backup::new(&src, &mut mem)?.run_to_completion(4096, std::time::Duration::ZERO, None)?;
        mem.pragma_update(None, "query_only", true)?;
        Ok(mem)
    };
    let out = read();
    let _ = std::fs::remove_dir_all(&scratch);
    out
}

/// `file:<path>?immutable=1`, with the characters a URI gives meaning to escaped. Windows paths
/// become `file:///C:/…` as SQLite expects.
fn immutable_uri(path: &Path) -> String {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let p = abs.to_string_lossy().replace('\\', "/");
    let mut uri = String::from(if p.starts_with('/') { "file://" } else { "file:///" });
    for c in p.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3f"),
            '#' => uri.push_str("%23"),
            ' ' => uri.push_str("%20"),
            _ => uri.push(c),
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

    /// A WAL-mode database like the MALDI `analysis.tsf` in the report, in a directory whose name
    /// needs escaping in a URI.
    fn wal_db(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mzpc-vsql {tag} #1 %{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("analysis.tsf");
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
            std::fs::copy(wal_of(&db), wal_of(&copy)).unwrap();
        }
        let listing = || {
            let mut v: Vec<(String, u64)> = std::fs::read_dir(&copy_dir).unwrap().map(|e| e.unwrap()).map(|e| (e.file_name().to_string_lossy().into_owned(), e.metadata().unwrap().len())).collect();
            v.sort();
            v
        };
        let before = listing();
        assert_eq!(before.len(), 2, "{before:?}");
        let c = open(&copy).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM Frames", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 5);
        drop(c);
        assert_eq!(listing(), before, "the open wrote into the folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn uri_escapes_and_makes_relative_paths_absolute() {
        assert_eq!(immutable_uri(Path::new("/a b/c#1?%.d/x.tdf")), "file:///a%20b/c%231%3f%25.d/x.tdf?immutable=1");
        let rel = immutable_uri(Path::new("run.d/analysis.tsf"));
        assert!(rel.starts_with("file:///") && rel.ends_with("/run.d/analysis.tsf?immutable=1"), "{rel}");
    }

    #[test]
    #[cfg(windows)]
    fn uri_for_a_windows_path() {
        assert_eq!(immutable_uri(Path::new(r"C:\Users\u\run d\analysis.tsf")), "file:///C:/Users/u/run%20d/analysis.tsf?immutable=1");
    }
}
