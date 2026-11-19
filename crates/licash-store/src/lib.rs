//! The SQLite file an actor of Licash keeps, and the rules every module
//! sharing it follows.
//!
//! An actor keeps one file: a hub its `hub.sqlite`, a wallet (and a swap
//! provider, which is one) its `wallet.sqlite`. Each module of the actor
//! owns tables of it, a *component*, with a connection of its own, and
//! makes them with [`component`]: a row of `schema_versions` tells that it
//! was, and which version of it. A module that finds no row has never been
//! used there, which proves nothing else; it never infers work from it.
//!
//! The file says what it is by SQLite's `application_id`, and is refused
//! when it is another's, or made by an earlier layout of Licash, which left
//! none: such a file is never opened, written or deleted here.
//!
//! Every connection is in WAL mode with `synchronous=FULL` and foreign keys
//! enforced, and every write is one IMMEDIATE transaction ([`write()`]).

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

/// What can go wrong with the file.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The file is not one this version opens, or cannot be made.
    #[error("{0}")]
    Setup(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Which actor's file it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Hub,
    Wallet,
}

impl Kind {
    /// The file's `application_id`: "LHUB" and "LWLT".
    fn id(self) -> i32 {
        match self {
            Kind::Hub => 0x4C48_5542,
            Kind::Wallet => 0x4C57_4C54,
        }
    }

    fn of(id: i32) -> Option<Self> {
        [Kind::Hub, Kind::Wallet].into_iter().find(|k| k.id() == id)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Hub => "hub",
            Kind::Wallet => "wallet",
        }
    }
}

/// How long a connection waits for another's write.
const BUSY: Duration = Duration::from_secs(10);

const VERSIONS: &str = "
CREATE TABLE schema_versions (
    component TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
";

/// Seconds since 1970.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A new file of `kind` at `path`, readable by its owner only, made so
/// before anything is written to it; never over an existing one. On an
/// error nothing is left behind.
pub fn create(path: &Path, kind: Kind) -> Result<Connection, Error> {
    private_file(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => Error::Setup(format!("{} exists", path.display())),
        _ => Error::Setup(format!("{}: {e}", path.display())),
    })?;
    let made = (|| {
        let mut conn = raw(path)?;
        configure(&conn)?;
        conn.pragma_update(None, "application_id", kind.id())?;
        write(&mut conn, |tx| {
            tx.execute_batch(VERSIONS)?;
            Ok::<_, Error>(())
        })?;
        Ok(conn)
    })();
    if made.is_err() {
        for file in [
            path.to_owned(),
            suffixed(path, "-wal"),
            suffixed(path, "-shm"),
        ] {
            let _ = std::fs::remove_file(file);
        }
    }
    made
}

/// The file of `kind` at `path`, which must exist and be made by this
/// layout: its connection writes.
pub fn open(path: &Path, kind: Kind) -> Result<Connection, Error> {
    if !path.is_file() {
        return Err(Error::Setup(format!(
            "no {} database at {}",
            kind.name(),
            path.display()
        )));
    }
    let conn = raw(path)?;
    verify(&conn, path, kind)?;
    configure(&conn)?;
    Ok(conn)
}

/// The file of `kind` at `path` for reading it as it is, by a connection
/// that cannot write: none if there is no file. A reader reaches the
/// tables its writer made, and no more: it creates and migrates nothing.
pub fn inspect(path: &Path, kind: Kind) -> Result<Option<Connection>, Error> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let conn = raw(path)?;
    verify(&conn, path, kind)?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(Some(conn))
}

/// Make `name`'s tables with `ddl`, as its `version`, unless they were.
/// Another version of them is refused: nothing is migrated.
pub fn component(conn: &mut Connection, name: &str, version: i64, ddl: &str) -> Result<(), Error> {
    if component_version(conn, name)? == Some(version) {
        return Ok(());
    }
    write(conn, |tx| component_in(tx, name, version, ddl))
}

/// [`component`] in the transaction `tx`, for a creator that fills the
/// tables in the same one it makes them in.
pub fn component_in(
    tx: &Transaction<'_>,
    name: &str,
    version: i64,
    ddl: &str,
) -> Result<(), Error> {
    let found: Option<i64> = tx
        .query_row(
            "SELECT version FROM schema_versions WHERE component = ?1",
            [name],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    match found {
        Some(v) if v == version => Ok(()),
        Some(v) => Err(Error::Setup(format!(
            "{name} is of version {v}, this build reads {version}"
        ))),
        None => {
            tx.execute_batch(ddl)?;
            tx.execute(
                "INSERT INTO schema_versions (component, version, created_at)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![name, version, now()],
            )?;
            Ok(())
        }
    }
}

/// The version of `name`'s tables: none if they were never made, as of a
/// module not yet used in this file.
pub fn component_version(conn: &Connection, name: &str) -> Result<Option<i64>, Error> {
    let mut stmt = conn.prepare("SELECT version FROM schema_versions WHERE component = ?1")?;
    let mut rows = stmt.query([name])?;
    Ok(match rows.next()? {
        Some(row) => Some(row.get(0)?),
        None => None,
    })
}

/// `f` in one IMMEDIATE transaction, committed if it succeeds.
///
/// A thread that writes the file while it has a write open on another
/// connection of it waits for itself until the busy timeout. Debug builds
/// panic at once instead.
pub fn write<T, E: From<rusqlite::Error>>(
    conn: &mut Connection,
    f: impl FnOnce(&Transaction<'_>) -> Result<T, E>,
) -> Result<T, E> {
    let tx = begin(conn)?;
    let value = f(&tx)?;
    tx.commit()?;
    Ok(value)
}

/// An IMMEDIATE transaction, rolled back unless committed: for a writer
/// that does more in it than [`write()`] lets it.
pub struct Tx<'c> {
    tx: Transaction<'c>,
    _held: Held,
}

impl<'c> std::ops::Deref for Tx<'c> {
    type Target = Transaction<'c>;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl Tx<'_> {
    pub fn commit(self) -> rusqlite::Result<()> {
        let Tx { tx, _held } = self;
        tx.commit()
    }
}

/// Begin an IMMEDIATE transaction on `conn`.
pub fn begin(conn: &mut Connection) -> rusqlite::Result<Tx<'_>> {
    let held = Held::enter(conn);
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    Ok(Tx { tx, _held: held })
}

/// A snapshot of `conn`'s file, committed rows and the WAL included, at
/// `to`, which must not exist: readable by its owner only, and synced.
pub fn backup(conn: &Connection, to: &Path) -> Result<(), Error> {
    let file = private_file(to).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => Error::Setup(format!("{} exists", to.display())),
        _ => Error::Setup(format!("{}: {e}", to.display())),
    })?;
    let copied = conn
        .backup(rusqlite::MAIN_DB, to, None)
        .map_err(Error::from)
        .and_then(|()| file.sync_all().map_err(Error::from));
    if copied.is_err() {
        let _ = std::fs::remove_file(to);
    }
    copied
}

/// `value` as SQLite's i64: an error beyond it, not a lesser amount.
pub fn sat(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value)
        .map_err(|_| rusqlite::Error::ToSqlConversionFailure(Box::new(OutOfRange(value))))
}

#[derive(Debug)]
struct OutOfRange(u64);

impl std::fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} does not fit a database integer", self.0)
    }
}

impl std::error::Error for OutOfRange {}

/// Refuse to go on while any of `names` is in `dir`: files of an earlier
/// layout, which this version keeps in `kept_in` and never reads. They
/// are named, and left as they are for the operator to move away.
pub fn refuse_legacy(dir: &Path, names: &[&str], kept_in: &str) -> Result<(), Error> {
    let found: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| dir.join(name).exists())
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(format!(
        "{} holds files of an earlier layout: {}. This version keeps all of it in {kept_in}: \
         move them away and start anew",
        dir.display(),
        found.join(", ")
    )))
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut file = OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(path)
}

fn raw(path: &Path) -> Result<Connection, Error> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(BUSY)?;
    Ok(conn)
}

/// That the file is of `kind` and of this layout, before anything is
/// changed in it.
fn verify(conn: &Connection, path: &Path, kind: Kind) -> Result<(), Error> {
    let id: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
    if id == kind.id() {
        return Ok(());
    }
    Err(Error::Setup(match (id, Kind::of(id)) {
        (0, _) => format!(
            "{} was not made by this layout of Licash (an earlier one, or not a Licash file): \
             it is left as it is; move it away and start anew",
            path.display()
        ),
        (_, Some(other)) => format!(
            "{} is a {} database, not a {} one",
            path.display(),
            other.name(),
            kind.name()
        ),
        (_, None) => format!("{} is not a Licash database", path.display()),
    }))
}

fn configure(conn: &Connection) -> Result<(), Error> {
    conn.pragma_update(None, "foreign_keys", true)?;
    let journal: String =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    let foreign: i64 = conn.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
    if !journal.eq_ignore_ascii_case("wal") || synchronous != 2 || foreign != 1 {
        return Err(Error::Setup(format!(
            "database durability: journal {journal}, synchronous {synchronous}, \
             foreign keys {foreign}"
        )));
    }
    Ok(())
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// The files this thread has a write open on, in debug builds.
struct Held(#[cfg(debug_assertions)] Option<PathBuf>);

#[cfg(debug_assertions)]
thread_local! {
    static HELD: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl Held {
    #[cfg(debug_assertions)]
    fn enter(conn: &Connection) -> Self {
        let Some(path) = conn.path().filter(|p| !p.is_empty()) else {
            return Held(None);
        };
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));
        HELD.with(|held| {
            let mut held = held.borrow_mut();
            assert!(
                !held.contains(&key),
                "{} is written while this thread has a write open on another connection of it: \
                 it would wait for itself",
                key.display()
            );
            held.push(key.clone());
        });
        Held(Some(key))
    }

    #[cfg(not(debug_assertions))]
    fn enter(_: &Connection) -> Self {
        Held()
    }
}

#[cfg(debug_assertions)]
impl Drop for Held {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            HELD.with(|held| {
                let mut held = held.borrow_mut();
                if let Some(at) = held.iter().rposition(|p| *p == key) {
                    held.remove(at);
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("a.sqlite")
    }

    #[test]
    fn a_new_file_is_private_durable_and_of_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let conn = create(&path, Kind::Hub).unwrap();
        let pragma =
            |name: &str| -> i64 { conn.pragma_query_value(None, name, |r| r.get(0)).unwrap() };
        assert_eq!(pragma("synchronous"), 2);
        assert_eq!(pragma("foreign_keys"), 1);
        assert_eq!(pragma("application_id") as i32, Kind::Hub.id());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(matches!(
            create(&path, Kind::Hub),
            Err(Error::Setup(m)) if m.contains("exists")
        ));
        drop(conn);
        open(&path, Kind::Hub).unwrap();
    }

    #[test]
    fn a_file_of_another_kind_or_layout_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        drop(create(&path, Kind::Wallet).unwrap());
        let wrong = open(&path, Kind::Hub).unwrap_err().to_string();
        assert!(wrong.contains("wallet database, not a hub one"), "{wrong}");

        // A file of an earlier layout: a user_version and no application_id,
        // in rollback-journal mode, which opening must not turn to WAL.
        let old = dir.path().join("old.sqlite");
        {
            let c = Connection::open(&old).unwrap();
            c.execute_batch("CREATE TABLE meta (one INTEGER); PRAGMA user_version = 16;")
                .unwrap();
        }
        let before = std::fs::read(&old).unwrap();
        for result in [
            open(&old, Kind::Wallet),
            inspect(&old, Kind::Wallet).map(|_| unreachable()),
        ] {
            assert!(result.unwrap_err().to_string().contains("earlier one"));
        }
        assert_eq!(std::fs::read(&old).unwrap(), before);
        assert!(!suffixed(&old, "-wal").exists());
    }

    fn unreachable() -> Connection {
        panic!("a legacy file was inspected")
    }

    #[test]
    fn inspecting_a_missing_file_finds_none_and_a_found_one_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        assert!(inspect(&path, Kind::Hub).unwrap().is_none());
        drop(create(&path, Kind::Hub).unwrap());
        let reader = inspect(&path, Kind::Hub).unwrap().unwrap();
        assert!(reader
            .execute("INSERT INTO schema_versions VALUES ('x', 1, 0)", [])
            .is_err());
    }

    #[test]
    fn a_component_is_made_once_and_another_version_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = create(&file(&dir), Kind::Hub).unwrap();
        assert_eq!(component_version(&conn, "deposits").unwrap(), None);
        let ddl = "CREATE TABLE deposits (hash BLOB PRIMARY KEY) STRICT;";
        component(&mut conn, "deposits", 1, ddl).unwrap();
        component(&mut conn, "deposits", 1, ddl).unwrap();
        assert_eq!(component_version(&conn, "deposits").unwrap(), Some(1));
        let refused = component(&mut conn, "deposits", 2, ddl).unwrap_err();
        assert!(refused
            .to_string()
            .contains("version 1, this build reads 2"));
        // Others stay unmade: one module's use proves nothing of another.
        assert_eq!(component_version(&conn, "inbox").unwrap(), None);
    }

    #[test]
    fn a_failing_component_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = create(&file(&dir), Kind::Hub).unwrap();
        let bad = "CREATE TABLE half (a INTEGER) STRICT; CREATE TABLE half (b INTEGER) STRICT;";
        assert!(component(&mut conn, "half", 1, bad).is_err());
        assert_eq!(component_version(&conn, "half").unwrap(), None);
        assert!(conn.prepare("SELECT a FROM half").is_err());
    }

    #[test]
    fn a_write_commits_on_success_and_rolls_back_on_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = create(&file(&dir), Kind::Hub).unwrap();
        component(&mut conn, "t", 1, "CREATE TABLE t (n INTEGER) STRICT;").unwrap();
        let failed: Result<(), Error> = write(&mut conn, |tx| {
            tx.execute("INSERT INTO t VALUES (1)", [])?;
            Err(Error::Setup("no".into()))
        });
        assert!(failed.is_err());
        let count = |c: &Connection| -> i64 {
            c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&conn), 0);
        write(&mut conn, |tx| {
            tx.execute("INSERT INTO t VALUES (1)", [])?;
            Ok::<_, Error>(())
        })
        .unwrap();
        assert_eq!(count(&conn), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "would wait for itself")]
    fn a_write_inside_a_write_on_another_connection_of_the_file_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut first = create(&path, Kind::Hub).unwrap();
        let mut second = open(&path, Kind::Hub).unwrap();
        let _ = write(&mut first, |_| write(&mut second, |_| Ok::<_, Error>(())));
    }

    #[test]
    fn writes_of_one_file_after_another_are_not_nested() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir);
        let mut first = create(&path, Kind::Hub).unwrap();
        let mut second = open(&path, Kind::Hub).unwrap();
        write(&mut first, |_| Ok::<_, Error>(())).unwrap();
        write(&mut second, |_| Ok::<_, Error>(())).unwrap();
        let tx = begin(&mut first).unwrap();
        tx.commit().unwrap();
        write(&mut second, |_| Ok::<_, Error>(())).unwrap();
    }

    #[test]
    fn an_amount_beyond_the_database_integer_is_an_error() {
        assert_eq!(sat(7).unwrap(), 7);
        assert_eq!(sat(i64::MAX as u64).unwrap(), i64::MAX);
        let e = sat(i64::MAX as u64 + 1).unwrap_err();
        assert!(e.to_string().contains("does not fit"), "{e}");
    }

    #[test]
    fn a_backup_is_private_whole_and_never_over_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = create(&file(&dir), Kind::Wallet).unwrap();
        component(&mut conn, "t", 1, "CREATE TABLE t (n INTEGER) STRICT;").unwrap();
        write(&mut conn, |tx| {
            tx.execute("INSERT INTO t VALUES (7)", [])?;
            Ok::<_, Error>(())
        })
        .unwrap();
        let to = dir.path().join("copy.sqlite");
        backup(&conn, &to).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&to).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let copy = open(&to, Kind::Wallet).unwrap();
        let n: i64 = copy.query_row("SELECT n FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 7);
        assert!(backup(&conn, &to).is_err());
    }

    #[test]
    fn legacy_files_are_named_and_left() {
        let dir = tempfile::tempdir().unwrap();
        assert!(refuse_legacy(dir.path(), &["a.sqlite", "b.sqlite"], "hub.sqlite").is_ok());
        std::fs::write(dir.path().join("b.sqlite"), b"x").unwrap();
        let e = refuse_legacy(dir.path(), &["a.sqlite", "b.sqlite"], "hub.sqlite")
            .unwrap_err()
            .to_string();
        assert!(e.contains("b.sqlite") && !e.contains("a.sqlite,"), "{e}");
        assert!(dir.path().join("b.sqlite").exists());
    }
}
