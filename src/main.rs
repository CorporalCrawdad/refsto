mod gui;
mod index;
use std::sync::Arc;
use gui::IndexingGui;
use sqlx::sqlite::SqlitePoolOptions;
use tokio::runtime;

const SQLITE_CON_CNT: u32 = 2048;
const HASH_SIZE_BYTES: usize = 8;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).unwrap_or(&String::from("./")).to_owned();
    let rt = Box::new(Arc::new(runtime::Builder::new_multi_thread().enable_time().build().unwrap()));
    let db_pool = rt.block_on(SqlitePoolOptions::new().max_connections(SQLITE_CON_CNT).connect(format!("sqlite:glowie.db?mode=rwc").as_str())).unwrap();
    let _ = eframe::run_native("Computing directory hashes...", eframe::NativeOptions::default(), Box::new(|cc| Box::new(IndexingGui::new(cc, dir, rt, db_pool))));
}
