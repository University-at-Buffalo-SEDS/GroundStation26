use super::*;
use sqlx::sqlite::SqliteConnectOptions;
use std::process::Command;

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("gs-sqlite-durability-{}-{}",std::process::id(),rand::random::<u128>()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn evidence(&self, label: &str) {
        if let Some(root) = std::env::var_os("GS_SQLITE_DURABILITY_EVIDENCE") {
            let root = PathBuf::from(root).join(format!("{label}-{}",std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            for entry in std::fs::read_dir(&self.0).unwrap() {
                let entry=entry.unwrap();
                if entry.file_type().unwrap().is_file() {std::fs::copy(entry.path(),root.join(entry.file_name())).unwrap();}
            }
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        if std::thread::panicking() { eprintln!("PRESERVED_SQLITE_FAILURE_DIRECTORY={}",self.0.display()); }
        else {let _=std::fs::remove_dir_all(&self.0);}
    }
}
async fn file_pool(directory:&Directory)->(SqlitePool,PathBuf) {
    let path=directory.0.join("recording.db");
    let pool=SqlitePoolOptions::new().max_connections(1).connect_with(SqliteConnectOptions::new().filename(&path).create_if_missing(true)).await.unwrap();
    for stmt in ["PRAGMA journal_mode=WAL;","PRAGMA wal_autocheckpoint=0;","PRAGMA busy_timeout=1;","CREATE TABLE original_samples(utc INTEGER,uptime INTEGER,value REAL);","INSERT INTO original_samples VALUES(1793744000123,1234,3.125);"] {
        sqlx::query(sqlx::AssertSqlSafe(stmt)).execute(&pool).await.unwrap();
    }
    (pool,path)
}
async fn held_reader(pool:&SqlitePool,path:&Path)->SqliteConnection {
    let mut reader=SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path)).await.unwrap();
    sqlx::query("BEGIN;").execute(&mut reader).await.unwrap();
    let first:i64=sqlx::query_scalar("SELECT COUNT(*) FROM original_samples;").fetch_one(&mut reader).await.unwrap();assert_eq!(first,1);
    sqlx::query("INSERT INTO original_samples VALUES(1793744000124,1235,4.25);").execute(pool).await.unwrap();
    assert!(PathBuf::from(format!("{}-wal",path.display())).metadata().unwrap().len()>0);
    reader
}
async fn release(reader:SqliteConnection) {
    let mut reader=reader;sqlx::query("ROLLBACK;").execute(&mut reader).await.unwrap();reader.close().await.unwrap();
}
async fn assert_rows(path:&Path) {
    let mut reader=SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(path)).await.unwrap();
    let rows=sqlx::query("SELECT utc,uptime,value FROM original_samples ORDER BY utc;").fetch_all(&mut reader).await.unwrap();
    assert_eq!(rows.len(),2,"every committed row must survive cleanup");
    assert_eq!((rows[0].get::<i64,_>(0),rows[0].get::<i64,_>(1),rows[0].get::<f64,_>(2)),(1793744000123,1234,3.125));
    assert_eq!((rows[1].get::<i64,_>(0),rows[1].get::<i64,_>(1),rows[1].get::<f64,_>(2)),(1793744000124,1235,4.25));reader.close().await.unwrap();
}
async fn scenario(case:&str) {
    let directory=Directory::new();let(pool,path)=file_pool(&directory).await;
    if case=="empty" {
        sqlx::query("DROP TABLE original_samples;").execute(&pool).await.unwrap();
        ensure_telemetry_schema(&pool).await.unwrap();
        assert!(!delete_sqlite_if_empty(path.to_str().unwrap()).await.unwrap(),"path-only emptiness cannot authorize unlink while a writer still owns the database");
        assert!(path.is_file());
        sqlx::query("INSERT INTO telemetry(timestamp_ms,source_timestamp_ms,data_type,sender_id,values_json,payload_json)VALUES(1793744000123,1234,'ORIGINAL','DAQ','[3.125]','[]');").execute(&pool).await.unwrap();
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM telemetry;").fetch_one(&pool).await.unwrap();assert_eq!(count,1);pool.close().await;
        let mut fresh=SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&path).create_if_missing(false)).await.unwrap();
        let row=sqlx::query("SELECT timestamp_ms,source_timestamp_ms,values_json FROM telemetry;").fetch_one(&mut fresh).await.unwrap();
        assert_eq!((row.get::<i64,_>(0),row.get::<i64,_>(1),row.get::<String,_>(2)),(1793744000123,1234,"[3.125]".into()));
        fresh.close().await.unwrap();return;
    }
    let reader=held_reader(&pool,&path).await;
    directory.evidence("before");
    if case=="close" {
        tokio::time::timeout(Duration::from_secs(30),close_and_finalize_sqlite(pool,path.to_str().unwrap())).await.unwrap();
    } else if case=="recover" {
        pool.close().await;
        tokio::time::timeout(Duration::from_secs(30),recover_sqlite_sidecars_in_dir(&directory.0)).await.unwrap().unwrap();
    } else if case=="reopen" {
        pool.close().await;
        let retained=directory.0.join("retained-main.sqlite");std::fs::rename(&path,&retained).unwrap();std::fs::create_dir(&path).unwrap();
        let wal=PathBuf::from(format!("{}-wal",path.display()));let before=std::fs::read(&wal).unwrap();
        tokio::time::timeout(Duration::from_secs(30),recover_sqlite_sidecars_in_dir(&directory.0)).await.unwrap().unwrap();
        directory.evidence("after-reopen-failure");
        assert_eq!(std::fs::read(&wal).expect("reopen failure must preserve the actual retained WAL"),before);
        std::fs::remove_dir(&path).unwrap();std::fs::rename(retained,&path).unwrap();
    } else if case=="positive" {
        release(reader).await;pool.close().await;
        finalize_sqlite_after_pool_close(path.to_str().unwrap()).await;
        assert_rows(&path).await;
        assert!(sqlite_sidecars_present(path.to_str().unwrap()).is_empty(),"uncontended SQLite must own standard sidecar cleanup");return;
    } else {panic!("unknown case")}
    directory.evidence("after");
    let wal=PathBuf::from(format!("{}-wal",path.display()));
    assert!(wal.is_file(),"uncertain cleanup must preserve WAL required by committed rows");
    release(reader).await;
    assert_rows(&path).await;
    finalize_sqlite_after_pool_close(path.to_str().unwrap()).await;
    assert_rows(&path).await;
    eprintln!("SQLITE_DURABILITY_{case}_PASS");
}
fn run(case:&str,name:&str) {
    if std::env::var("GS_SQLITE_DURABILITY_CASE").as_deref()==Ok(case) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(scenario(case));return;
    }
    let output=Command::new(std::env::current_exe().unwrap()).args(["--exact",&format!("telemetry_db::durability_tests::{name}"),"--nocapture","--test-threads=1"]).env("GS_SQLITE_DURABILITY_CASE",case).env("GS_SQLITE_SHUTDOWN_PRAGMA_RETRIES","1").env("GS_SQLITE_SHUTDOWN_PRAGMA_RETRY_DELAY_MS","10").output().unwrap();
    assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
}
#[test]fn actual_close_preserves_held_reader_committed_wal(){run("close","actual_close_preserves_held_reader_committed_wal");}
#[test]fn actual_recovery_preserves_held_reader_committed_wal(){run("recover","actual_recovery_preserves_held_reader_committed_wal");}
#[test]fn actual_reopen_failure_preserves_original_wal(){run("reopen","actual_reopen_failure_preserves_original_wal");}
#[test]fn path_only_empty_cleanup_cannot_unlink_an_owned_database(){run("empty","path_only_empty_cleanup_cannot_unlink_an_owned_database");}
#[test]fn uncontended_sqlite_cleanup_preserves_original_rows_and_clocks(){run("positive","uncontended_sqlite_cleanup_preserves_original_rows_and_clocks");}
