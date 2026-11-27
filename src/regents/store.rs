//! SQLite access shared by the workspace databases (channels, threads). The
//! server is the only writer (ADR-0003).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// Time that an access waits for a lock that another connection holds. The
/// server is the only writer, so a lock is rare and short. The database I/O
/// runs on the server loop: keep this small until it moves to a worker.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);

/// The schema of a database: `sql` creates version `version` from an empty
/// database. `PRAGMA user_version` records the version.
pub(crate) struct Schema {
    pub(crate) version: i64,
    pub(crate) sql: &'static str,
}

/// Opens `path` for writing, creates it when it is missing and brings its
/// schema to `schema.version`.
pub(crate) fn open_writable(path: &Path, schema: &Schema) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < schema.version {
        connection.execute_batch(&format!(
            "BEGIN;\n{}\nPRAGMA user_version = {};\nCOMMIT;",
            schema.sql, schema.version
        ))?;
    }
    Ok(connection)
}

/// Opens an existing database for reading.
pub(crate) fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    Ok(connection)
}

/// A temporary directory for tests, removed on drop.
#[cfg(test)]
pub(crate) struct TempFolder(pub(crate) std::path::PathBuf);

#[cfg(test)]
impl TempFolder {
    pub(crate) fn new(name: &str) -> Self {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("regents-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

#[cfg(test)]
impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
