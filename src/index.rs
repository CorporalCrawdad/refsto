use std::{time::SystemTime, path::Path, fs::File, io::{BufReader, Read}, sync::Arc};
use futures::{stream::FuturesUnordered, StreamExt};
use sqlx::{Row, sqlite::SqliteQueryResult};
use tokio::{fs::metadata, sync::RwLock};
use tokio_util::sync::CancellationToken;
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
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
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
                    let phash = image_hasher::HasherConfig::with_bytes_type::<[u8; crate::HASH_SIZE_BYTES]>().to_hasher().hash_image(&img_bytes).to_base64();
                    sqlx::query("INSERT OR REPLACE INTO entries (filepath, phash, filesize, lastmod) VALUES (?, ?, ?, ?)").bind(filepath).bind(phash).bind(filesize).bind(lastmod).execute(&mut conn).await.or_else(|_| Err(HashIndexError::Other))
                },
                Err(image::ImageError::Unsupported(_)) => return Err(HashIndexError::Format),
                Err(image::ImageError::IoError(_)) => return Err(HashIndexError::Encoding),
                Err(image::ImageError::Decoding(_)) => return Err(HashIndexError::Encoding),
                Err(_) => panic!(),
            }
        }.await
    }

    pub async fn find_dupes(&self, hash_dupes: Arc<RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<RwLock<Vec<Vec<String>>>>, cancel_token: CancellationToken) {
        let mut conn = loop {
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
            }
        };

        // get hash dupes
        let mut hash_dupes = hash_dupes.write().await;
        if let Ok(groupby_result) = sqlx::query("SELECT phash, COUNT(rowid) as collisioncnt FROM entries GROUP BY phash HAVING collisioncnt > 1;").fetch_all(&mut conn).await {
            for row in groupby_result {
                let phash: String = row.get("phash");
                let mut filepaths: Vec<String> = vec!();
                if let Ok(collided_filenames) = sqlx::query("SELECT filepath FROM entries WHERE phash = ?;").bind(&phash).fetch_all(&mut conn).await {
                    for row in collided_filenames {
                        filepaths.push(row.get("filepath"));
                    }
                } else {
                    eprintln!("Error with query!");
                }
                println!("hash duped: {:?}", filepaths);
                hash_dupes.push(filepaths);
            }
        }

        let mut fut_set = FuturesUnordered::new();
        let mut bin_dupes = bin_dupes.write().await;
        for dupe_set in (&hash_dupes).iter() {
            let dupe_set = dupe_set.clone();
            fut_set.push(async {
                let mut exact_matches: Vec<Vec<String>> = vec!();
                for candidate in dupe_set {
                    for (i, reference) in exact_matches.iter().enumerate() {
                        if is_same_file(&reference[0], &candidate).unwrap_or(true) {
                            println!("{} and {} binary match!", reference[0], candidate);
                            exact_matches[i].push(candidate.to_string());
                            break;
                        }
                    }
                    exact_matches.push(vec!(candidate.to_string()));
                }
                exact_matches
            });
        }
        while let Some(result) = fut_set.next().await {
            if cancel_token.is_cancelled() {
                break;
            }
            for bin_set in result {
                if bin_set.len() > 1 {
                    println!("bin duped: {:?}", bin_set);
                    bin_dupes.push(bin_set);
                }
            }
        }
    }

    pub async fn cluster(&self, hash_dupes: Arc<RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<RwLock<Vec<Vec<String>>>>, hamming_range: usize, cancel_token: CancellationToken) {
        // Hamming range provided as 0 - 100 percentile difference.
        self.find_dupes(hash_dupes, bin_dupes, cancel_token).await;
    }
}

fn is_same_file(left: impl AsRef<Path>, right: impl AsRef<Path>) -> Result<bool, std::io::Error> {
    let fleft = File::open(left)?;
    let fright = File::open(right)?;

    if fleft.metadata().unwrap().len() != fright.metadata().unwrap().len() {
        return Ok(false);
    }

    let fleft = BufReader::new(fleft);
    let fright = BufReader::new(fright);

    for (bleft, bright) in fleft.bytes().zip(fright.bytes()) {
        if bleft.unwrap() != bright.unwrap() {
            return Ok(false);
        }
    }
    Ok(true)
}