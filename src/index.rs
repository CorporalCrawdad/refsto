use std::time::SystemTime;
use sqlx::{Row, sqlite::SqliteQueryResult};
use tokio::fs::metadata;

pub struct HashIndexer {
    db_pool: sqlx::SqlitePool,
    // hasher: img_hash::Hasher,   
}

impl HashIndexer {
    pub fn new(db_pool: sqlx::SqlitePool) -> Self {
        // let hasher = img_hash::HasherConfig::new().to_hasher();
        HashIndexer{db_pool}
    }

    pub async fn update(&self, filepath: String) -> Result<SqliteQueryResult, anyhow::Error> {
        let filepath = filepath.as_str();
        let meta = metadata(filepath).await?;
        let filesize = meta.len() as i64;
        let lastmod = match meta.modified() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
        };
        // check heuristic diff before updating hash
        // if lastmod or filesize different, rehash
        let mut conn = self.db_pool.acquire().await?;
        if let Ok(rows) = sqlx::query("SELECT (phash, filesize, lastmod) FROM entries WHERE filepath = ?").bind(filepath).fetch_all(&mut conn).await {
            if rows.len() > 0 { // filepath already in db, conditionally compute hash and update
                if rows.len() > 1 {
                    return Err(anyhow::anyhow!("Multiple entries for {} in db", &filepath));
                } else {
                    let (db_filesize, db_lastmod): (i64, i64) = (rows[0].try_get("filesize")?, rows[0].try_get("lastmod")?);
                    if db_filesize == filesize && db_lastmod == lastmod {
                        return Ok(SqliteQueryResult::default());
                    }
                }
            }
        }
        let phash = img_hash::HasherConfig::new().to_hasher().hash_image(&img_hash::image::open(filepath)?).as_bytes().to_owned();
        sqlx::query("INSERT OR REPLACE INTO entries (? ? ? ?)").bind(filepath).bind(phash).bind(filesize).bind(lastmod).execute(&mut conn).await.or_else(|x| Err(anyhow::anyhow!(x)))
    }
}
