use std::time::SystemTime;
use sqlx::{SqlitePool, Row, sqlite::SqliteQueryResult};
use tokio::fs::metadata;

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

    pub async fn update(self, filepath: &str) -> Result<SqliteQueryResult, anyhow::Error> {
        let meta = metadata(filepath).await?;
        let filesize = meta.len() as i64;
        let lastmod = match meta.modified() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
        };
        // check heuristic diff before updating hash
        // if lastmod or filesize different, rehash
        let mut conn = self.db_con_pool.acquire().await?;
        if let Ok(rows) = sqlx::query("SELECT (phash, filesize, lastmod) FROM entries WHERE filepath = ?").bind(filepath).fetch_all(&mut conn).await {
            if rows.len() > 0 { // filepath already in db, conditionally compute hash and update
                if rows.len() > 1 {
                    return Err(anyhow::anyhow!("Multiple entries for {filepath} in db"));
                } else {
                    let (db_filesize, db_lastmod): (i64, i64) = (rows[0].try_get("filesize")?, rows[0].try_get("lastmod")?);
                    if db_filesize == filesize && db_lastmod == lastmod {
                        return Ok(SqliteQueryResult::default());
                    }
                }
            }
        }
        let phash = self.hasher.hash_image(&img_hash::image::open(filepath)?).as_bytes().to_owned();
        sqlx::query("INSERT OR REPLACE INTO entries (? ? ? ?)").bind(filepath).bind(phash).bind(filesize).bind(lastmod).execute(&mut conn).await.or_else(|x| Err(anyhow::anyhow!(x)))
    }
}
