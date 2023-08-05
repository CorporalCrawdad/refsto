use std::{time::{SystemTime, Duration}, path::Path, fs::File, io::{BufReader, Read}, sync::Arc};
use futures::stream::FuturesUnordered;
use sqlx::{Row, sqlite::SqliteQueryResult, Acquire};
use tokio::{fs::metadata, sync::RwLock};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::HASH_SIZE_BYTES;

pub struct HashIndexer {
    db_pool: sqlx::SqlitePool,
    hasher_config: image_hasher::HasherConfig<[u8; crate::HASH_SIZE_BYTES]>,
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
        let hasher_config = image_hasher::HasherConfig::with_bytes_type::<[u8; crate::HASH_SIZE_BYTES]>();
        HashIndexer{db_pool, hasher_config}
    }

    pub async fn update(&self, filepath: String, tasknum: usize) -> Result<SqliteQueryResult, HashIndexError> {
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
        if let Ok(rows) = sqlx::query("SELECT phash, filesize, lastmod FROM entries WHERE filepath = ?").bind(filepath).fetch_all(conn.acquire().await.unwrap()).await {
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
        let res = async move {
            let img_bytes = image::io::Reader::open(filepath).unwrap().with_guessed_format().unwrap().decode();
            match img_bytes {
                Ok(img_bytes) => {
                    let phash = self.hasher_config.to_hasher().hash_image(&img_bytes).to_base64();
                    // println!("Task {} hashing complete", tasknum);
                    let res = sqlx::query("INSERT OR REPLACE INTO entries (filepath, phash, filesize, lastmod) VALUES (?, ?, ?, ?)").bind(filepath).bind(phash).bind(filesize).bind(lastmod).execute(conn.acquire().await.unwrap()).await.or_else(|_| Err(HashIndexError::Other));
                    // println!("Task {} query completed", tasknum);
                    res
                },
                Err(image::ImageError::Unsupported(_)) => return Err(HashIndexError::Format),
                Err(image::ImageError::IoError(_)) => return Err(HashIndexError::Encoding),
                Err(image::ImageError::Decoding(_)) => return Err(HashIndexError::Encoding),
                Err(_) => panic!(),
            }
        }.await;
        // println!("Task {} complete", tasknum);
        res
    }

    pub async fn find_dupes(&self, hash_dupes: Arc<RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<RwLock<Vec<Vec<String>>>>, cancel_token: CancellationToken) {
        let mut conn = loop {
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
            }
        };

        // get hash dupes
        let mut hash_dupes = hash_dupes.write().await;
        if let Ok(groupby_result) = sqlx::query("SELECT phash, COUNT(rowid) as collisioncnt FROM entries GROUP BY phash HAVING collisioncnt > 1;").fetch_all(conn.acquire().await.unwrap()).await {
            for row in groupby_result {
                let phash: String = row.get("phash");
                let mut filepaths: Vec<String> = vec!();
                if let Ok(collided_filenames) = sqlx::query("SELECT filepath FROM entries WHERE phash = ?;").bind(&phash).fetch_all(conn.acquire().await.unwrap()).await {
                    for row in collided_filenames {
                        filepaths.push(row.get("filepath"));
                    }
                } else {
                    eprintln!("Error with query!");
                }
                // println!("hash duped: {:?}", filepaths);
                hash_dupes.push(filepaths);
            }
        }

        let fut_set = FuturesUnordered::new();
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
        let mut fut_set = fut_set.timeout_repeating(tokio::time::interval(Duration::from_secs(5)));
        while let Ok(Some(result)) = fut_set.try_next().await {
            if cancel_token.is_cancelled() {
                break;
            }
            for bin_set in result {
                if bin_set.len() > 1 {
                    // println!("bin duped: {:?}", bin_set);
                    bin_dupes.push(bin_set);
                }
            }
        }
    }

    pub async fn cluster(&self, hash_dupes: Arc<RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<RwLock<Vec<Vec<String>>>>, hamming_range: usize, cancel_token: CancellationToken) {
        // Hamming range provided as 0 - 100 percentile difference.
        // self.find_dupes(hash_dupes.clone(), bin_dupes, cancel_token).await;
        let hamming_distance = (hamming_range * HASH_SIZE_BYTES * 8) / 100;
        let hamming_distance: u32 = hamming_distance.try_into().unwrap();
        println!("Checking for distance {}", hamming_distance);

        let mut conn = loop {
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
            }
        };
        let mut hash_dupes = hash_dupes.write().await;
        let mut hash_dupes_phashes: Vec<image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>> = vec![];
        for dupe_set in (*hash_dupes).iter() {
            let set_phash = sqlx::query("SELECT phash FROM entries WHERE filepath = ?").bind(&dupe_set[0]).fetch_one(conn.acquire().await.unwrap()).await.unwrap();
            hash_dupes_phashes.push(image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>::from_base64(set_phash.get("phash")).unwrap());
        }
        assert_eq!(hash_dupes.len(), hash_dupes_phashes.len());

        let mut all_entries = sqlx::query("SELECT * FROM entries;").fetch(conn.acquire().await.unwrap());
        while let Some(Ok(row)) = all_entries.next().await {
            let mut matched = false;
            let phash = image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>::from_base64(row.get("phash")).unwrap();
            for (idx,reference) in hash_dupes_phashes.iter().enumerate() {
                if phash.dist(&reference) <= hamming_distance {
                    hash_dupes[idx].push(row.get::<String, &str>("filepath"));
                    matched = true;
                    break;
                }
            }
            if !matched {
                hash_dupes.push(vec!(row.get::<String, &str>("filepath")));
                hash_dupes_phashes.push(phash);
                assert_eq!(hash_dupes.len(), hash_dupes_phashes.len());
            }
        }
        hash_dupes.retain(|x| x.len()>1);
        for set in hash_dupes.iter() {
            println!("Dupe set: {:?}", set);
        }
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