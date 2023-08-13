mod gui;
mod index;
use std::sync::Arc;
use dirs::config_local_dir;
use sqlx::{sqlite::SqlitePoolOptions, Row};
use tokio::runtime;

use gui::IndexingGui;

const SQLITE_CON_CNT: u32 = 2048;
const HASH_SIZE_BYTES: usize = 8;
const TABLE_VERSION: i64 = 1;

async fn setup_database(pool: sqlx::SqlitePool) {
    if let Ok(table_version) = sqlx::query("SELECT table_version FROM metadata").fetch_one(&pool).await {
        if table_version.get::<i64,_>("table_version") == TABLE_VERSION {
            return
        } else {
            todo!("Present update options for incompatible database version");
        }
    }
    let _ = sqlx::query("CREATE TABLE IF NOT EXISTS metadata (refsto_version STRING, table_version INTEGER);
    CREATE TABLE IF NOT EXISTS watched_dirs ( rowid INTEGER PRIMARY KEY ASC, fullpath TEXT UNIQUE );
    CREATE TABLE IF NOT EXISTS entries ( entry_id INTEGER PRIMARY KEY ASC, filepath TEXT UNIQUE, phash BLOB, xxhash BLOB, filesize INTEGER, lastmod INTEGER, ignored BOOLEAN DEFAULT 0 );
    CREATE TABLE IF NOT EXISTS hash_dupe_sets ( hdset_id INTEGER PRIMARY KEY ASC, hamming_distance INTEGER);
    CREATE TABLE IF NOT EXISTS hash_dupe_sets_x_entries ( hdset_id INTEGER, entry_id INTEGER );").execute(&pool).await;
}

fn main() {
    // std::env::set_var("WINIT_UNIX_BACKEND", "x11"); // currently necessary since winit does not support DnD in Wayland
    let rt = Arc::new(runtime::Builder::new_multi_thread().enable_time().build().unwrap());
    let db_pool = rt.block_on(SqlitePoolOptions::new().max_connections(SQLITE_CON_CNT).connect(format!("sqlite:{}/refsto.dat?mode=rwc", config_local_dir().unwrap().to_string_lossy()).as_str())).unwrap();
    rt.block_on(setup_database(db_pool.clone()));
    let _ = eframe::run_native("Computing directory hashes...", eframe::NativeOptions::default(), Box::new(|cc| Box::new(IndexingGui::new(cc, rt, db_pool))));
}
