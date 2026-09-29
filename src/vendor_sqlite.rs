//! Opening a vendor SQLite database (Bruker `analysis.tsf` / `.tdf` / `.baf` caches, HyStar traces)
//! without writing anything into the user's raw data folder.
//!
//! `SQLITE_OPEN_READ_ONLY` is not enough: on a database in WAL mode a read-only connection creates
//! `<db>-shm` and `<db>-wal` beside it and, unable to checkpoint, leaves them there — which is what a
//! Bruker MALDI TSF conversion did to the issue author's `.d` (HUPO-PSI/mzPeak-specification#23).
//! `immutable=1` tells SQLite the file cannot change, so it creates nothing and takes no locks.
//!
//! The catch: an immutable open also ignores the WAL, and a `-wal` left non-empty by the acquisition
//! software holds committed rows. Then the database is read the ordinary read-only way, which sees
//! them; SQLite may maintain the existing side files, but it does not create a WAL that was not there.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Open `path` read-only, creating no side file (see the module docs).
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if has_wal_content(path) {
        log::warn!(
            "{} has a non-empty -wal beside it (committed rows the acquisition software never \
             checkpointed); reading it through the WAL",
            path.display()
        );
        return Connection::open_with_flags(path, flags);
    }
    Connection::open_with_flags(immutable_uri(path), flags | OpenFlags::SQLITE_OPEN_URI)
}

fn has_wal_content(path: &Path) -> bool {
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).is_ok_and(|m| m.len() > 0)
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
        drop(c);
        drop(writer);
        let _ = std::fs::remove_dir_all(db.parent().unwrap());
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
