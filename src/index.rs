use std::time::SystemTime;
use sqlx::{SqlitePool, Row};
use tokio::fs::metadata;
use tokio_stream::StreamExt;

struct HashIndexer {
    db_con_pool: sqlx::SqlitePool,
    hasher: img_hash::Hasher,   
}

impl HashIndexer {
    pub async fn new() -> Result<HashIndexer, sqlx::Error> {
        HashIndexer::new_named("glowie.db").await
    }

    pub async fn new_named(db_name: &str) -> Result<HashIndexer, sqlx::Error> {
        let db_con_pool = SqlitePool::connect(format!("sqlite:{}",db_name).as_str()).await?;
        // uses standard Gradient hashing algorithm
        let hasher = img_hash::HasherConfig::new().to_hasher();
        Ok(HashIndexer{db_con_pool, hasher})
    }

    pub async fn update(self, filepath: &str) -> Result<(), anyhow::Error> {
        let meta = metadata(filepath).await?;
        let filesize = meta.len();
        let lastmod = match meta.modified() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap(),
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap(),
        };
        // check heuristic diff before updating hash
        // if lastmod or filesize different, rehash
        let mut conn = self.db_con_pool.acquire().await?;
        let mut rows = sqlx::query("SELECT (phash, filesize, lastmod) FROM entries WHERE filepath = ?").bind(filepath).fetch(&mut conn);
        while let Some(row) = rows.try_next().await? {
            let (db_filepath, db_phash, db_filesize, db_lastmod): (&str, &[u8], u32, &[u8]) = (row.try_get("filepath")?, row.try_get("phash")?, row.try_get("filesize")?, row.try_get("lastmod")?);
            if db_lastmod != lastmod || db_filesize != filesize {

            }
        }
        let phash = self.hasher.hash_image(&img_hash::image::open(filepath)?).as_bytes();
        Ok(())
    }
}
