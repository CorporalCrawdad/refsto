use std::time::SystemTime;
use sqlx::{Row, sqlite::SqliteQueryResult};
use tokio::fs::metadata;
pub struct HashIndexer {
    db_pool: sqlx::SqlitePool,
    // hasher: img_hash::Hasher,   
}

pub enum HashIndexError {
    Format,
    Encoding,
    MalformedDB,
    Other,
}

impl HashIndexer {
    pub fn new(db_pool: sqlx::SqlitePool) -> Self {
        // let hasher = img_hash::HasherConfig::new().to_hasher();
        HashIndexer{db_pool}
    }

    pub async fn update(&self, filepath: String) -> Result<SqliteQueryResult, HashIndexError> {
        let filepath = filepath.as_str();
        let meta = { match metadata(filepath).await {
            Ok(x) => x,
            Err(_) => return Err(HashIndexError::Other),
        }};
        let filesize = meta.len() as i64;
        let lastmod = match meta.modified() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
        };
        // check heuristic diff before updating hash
        // if lastmod or filesize different, rehash
        let mut conn = loop {
            let acquisition = self.db_pool.acquire().await;
            if acquisition.is_ok() {
                break acquisition.unwrap();
            }
        };
        if let Ok(rows) = sqlx::query("SELECT phash, filesize, lastmod FROM entries WHERE filepath = ?").bind(filepath).fetch_all(&mut conn).await {
            if rows.len() > 0 { // filepath already in db, conditionally compute hash and update
                if rows.len() > 1 {
                    return Err(HashIndexError::MalformedDB);
                } else {
                    let (db_filesize, db_lastmod): (i64, i64) = (rows[0].try_get("filesize").unwrap(), rows[0].try_get("lastmod").unwrap());
                    if db_filesize == filesize && db_lastmod == lastmod {
                        return Ok(SqliteQueryResult::default());
                    }
                }
            } else {
            }
        }
        async move {
            let img_bytes = image::io::Reader::open(filepath).unwrap().with_guessed_format().unwrap().decode();
            match img_bytes {
                Ok(img_bytes) => {
                    let phash = image_hasher::HasherConfig::new().to_hasher().hash_image(&img_bytes).to_base64();
                    sqlx::query("INSERT OR REPLACE INTO entries (filepath, phash, filesize, lastmod) VALUES (?, ?, ?, ?)").bind(filepath).bind(phash).bind(filesize).bind(lastmod).execute(&mut conn).await.or_else(|_| Err(HashIndexError::Other))
                },
                Err(image::ImageError::Unsupported(_)) => return Err(HashIndexError::Format),
                Err(image::ImageError::IoError(_)) => return Err(HashIndexError::Encoding),
                Err(image::ImageError::Decoding(_)) => return Err(HashIndexError::Encoding),
                Err(_) => panic!(),
            }
        }.await
    }
}