use sqlx::{sqlite,Connection, SqlitePool};
use img_hash;

struct HashIndexer {
    dbcon: sqlx::SqlitePool,
    
}

impl HashIndexer {
    pub async fn new() -> Result<HashIndexer, sqlx::Error> {
        HashIndexer::new_named("glowie.db").await
    }

    pub async fn new_named(db_name: &str) -> Result<HashIndexer, sqlx::Error> {
        let dbcon = SqlitePool::connect(format!("sqlite:{}",db_name).as_str()).await?;
        Ok(HashIndexer{dbcon})
    }

    pub async fn update(self, filepath: &str, phash: &[u8]) -> Result<(), ()> {
        self.dbcon.execute()
    }
}
