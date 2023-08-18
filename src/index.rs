use std::{time::SystemTime, path::PathBuf};
// use futures::stream::FuturesUnordered;
use sqlx::{Row, sqlite::SqliteQueryResult, Acquire};
use tokio::fs::metadata;
use xxhash_rust::xxh3::xxh3_64;

use crate::HASH_SIZE_BYTES;
use crate::gui::{BinDupeMessage, KeepWhichFile};

pub struct HashIndexer {
    db_pool: sqlx::SqlitePool,
    hasher_config: image_hasher::HasherConfig<[u8; HASH_SIZE_BYTES]>,
}

pub enum HashIndexError {
    Format,
    Encoding,
    MalformedDB,
    InsertDB,
    FileNotFound,
    Other,
}

impl HashIndexer {
    pub fn new(db_pool: sqlx::SqlitePool) -> Self {
        // let hasher = img_hash::HasherConfig::new().to_hasher();
        let hasher_config = image_hasher::HasherConfig::with_bytes_type::<[u8; HASH_SIZE_BYTES]>();
        HashIndexer{db_pool, hasher_config}
    }

    pub async fn update(&self, fullpath: String) -> Result<SqliteQueryResult, HashIndexError> {
        let fullpath = fullpath.as_str();
        let mut file_bytes = None;
        let meta = { match metadata(fullpath).await {
            Ok(x) => x,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => return Err(HashIndexError::FileNotFound),
            Err(e) => {
                eprintln!("{}: {:?}", fullpath, e);
                return Err(HashIndexError::Other)
            },
        }};
        let filesize = meta.len() as i64;
        let mtime = match meta.modified() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
        };
        let ctime = match meta.created() {
            Ok(time) => time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
            Err(_) => SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64,
        };
        let pb = PathBuf::from(fullpath);
        let filename: String = pb.file_name().unwrap().to_string_lossy().into();
        let dircnt = pb.ancestors().count() as i64;
        // check heuristic diff before updating hash
        // if mtime or filesize different, rehash
        let mut conn = loop {
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
            }
        };
        if let Ok(rows) = sqlx::query("SELECT phash, xxhash, filesize, mtime, ignored FROM entries WHERE fullpath = ?").bind(fullpath).fetch_all(conn.acquire().await.unwrap()).await {
            if rows.len() > 0 { // fullpath already in db, conditionally compute hash and update
                if rows.len() > 1 {
                    return Err(HashIndexError::MalformedDB);
                } else {
                    let (db_filesize, db_mtime): (i64, i64) = (rows[0].get("filesize"), rows[0].get("mtime"));
                    if db_filesize == filesize && db_mtime == mtime {
                        return Ok(SqliteQueryResult::default());
                    }
                    // let (db_filesize, db_mtime, db_xxhash, ignored): (i64, i64, i64, bool) = (rows[0].get("filesize"), rows[0].get("mtime"), rows[0].get("xxhash"), rows[0].get("ignored"));
                    // let db_xxhash = u64::from_be_bytes(db_xxhash.to_be_bytes());
                    // Do we want to load file and check xxhash every time in any case?
                    // else {
                    //     // pessimistically skip files previously deemed not-images
                    //     if ignored {
                    //         return Ok(SqliteQueryResult::default());
                    //     }
                    //     file_bytes = Some(tokio::fs::read(fullpath).await.unwrap());
                    //     xxhash = Some(xxh3_64(file_bytes.as_ref().unwrap().as_ref()));
                    //     if db_xxhash == xxhash.unwrap() {
                    //         return Ok(SqliteQueryResult::default());
                    //     } else {
                    //         println!("XXHASH MISMATCH FOR {}", fullpath);
                    //     }
                    // }
                }
            }
        }
        let res = async move {
            if file_bytes.is_none() { file_bytes = Some(tokio::fs::read(fullpath).await.expect(&format!("ERROR READING FILE {}", fullpath))) };
            let xxhash = i64::from_be_bytes(xxh3_64(&file_bytes.as_ref().unwrap()).to_be_bytes());
            let img_bytes = image::load_from_memory(&file_bytes.as_ref().unwrap());
            match img_bytes {
                Ok(img_bytes) => {
                    let phash = self.hasher_config.to_hasher().hash_image(&img_bytes).to_base64();
                    let res = sqlx::query("INSERT OR REPLACE INTO entries (fullpath, phash, xxhash, filesize, mtime, ctime, filename, dircnt) VALUES (?, ?, ?, ?, ?, ?, ?, ?)").bind(fullpath).bind(phash).bind(xxhash).bind(filesize).bind(mtime).bind(ctime).bind(filename).bind(dircnt).execute(conn.acquire().await.unwrap()).await.or_else(|_| Err(HashIndexError::InsertDB));
                    res
                },
                Err(image::ImageError::Unsupported(_)) => {
                    let _ = sqlx::query("INSERT OR REPLACE INTO entries (fullpath, xxhash, filesize, mtime, ctime, filename, dircnt, ignored) VALUES (?, ?, ?, ?, ?, ?, ?, 1)").bind(fullpath).bind(xxhash).bind(filesize).bind(mtime).bind(ctime).bind(filename).bind(dircnt).execute(conn.acquire().await.unwrap()).await.or_else(|_| Err(HashIndexError::InsertDB));
                    return Err(HashIndexError::Format)
                },
                Err(image::ImageError::IoError(_)) => return Err(HashIndexError::Encoding),
                Err(image::ImageError::Decoding(_)) => return Err(HashIndexError::Encoding),
                Err(_) => panic!(),
            }
        }.await;
        res
    }

    pub async fn find_bindupes(&self, incl_ignored: bool, method: KeepWhichFile, reversed: bool, tx: std::sync::mpsc::Sender<BinDupeMessage>) {
        let mut conn = loop {
            if let Ok(acquisition) = self.db_pool.acquire().await {
                break acquisition;
            }
        };
        let conn = conn.acquire().await.unwrap();

        let query_str = {
            if incl_ignored {
                "SELECT xxhash, COUNT(rowid) as collisioncnt FROM entries GROUP BY xxhash HAVING collisioncnt > 1;"
            } else {
                "SELECT xxhash, COUNT(rowid) as collisioncnt FROM entries WHERE ignored = 0 GROUP BY xxhash HAVING collisioncnt > 1;"
            }
        };
        let collision_rows: Vec<i64> = sqlx::query(query_str)
                .fetch_all(&mut *conn).await
                .expect("SELECT duplicated xxhash from database failed!")
                .iter().map(|x| x.get::<i64,_>("xxhash")).collect();

        for i64_xxhash in collision_rows {
            println!("Set of xxhash: {}", i64_xxhash);
            tx.send(BinDupeMessage::NewSet).unwrap();
            sqlx::query(
                format!(
                        "SELECT fullpath FROM entries WHERE xxhash = ? ORDER BY {}{};",
                        method.get_query(),
                        {if reversed {" DESC"} else {""}}
                    )
                    .as_str()
                )
                .bind(i64_xxhash)
                .fetch_all(&mut *conn).await.unwrap().iter()
                .for_each(|x| { println!("{}\t{}", i64_xxhash, x.get::<String,_>("fullpath")); tx.send(BinDupeMessage::Entry(PathBuf::from(x.get::<String,_>("fullpath")))).unwrap();});
        }
        // tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
    }

    //     // get hash dupes
    //     let mut hash_dupes = hash_dupes.write().await;
    //     if let Ok(groupby_result) = sqlx::query("SELECT phash, COUNT(rowid) as collisioncnt FROM entries GROUP BY phash HAVING collisioncnt > 1;").fetch_all(conn.acquire().await.unwrap()).await {
    //         for row in groupby_result {
    //             let phash: String = row.get("phash");
    //             let mut filepaths: Vec<String> = vec!();
    //             if let Ok(collided_filenames) = sqlx::query("SELECT filepath FROM entries WHERE phash = ?;").bind(&phash).fetch_all(conn.acquire().await.unwrap()).await {
    //                 for row in collided_filenames {
    //                     filepaths.push(row.get("filepath"));
    //                 }
    //             } else {
    //                 eprintln!("Error with query!");
    //             }
    //             // println!("hash duped: {:?}", filepaths);
    //             hash_dupes.push(filepaths);
    //         }
    //     }

    //     let fut_set = FuturesUnordered::new();
    //     let mut bin_dupes = bin_dupes.write().await;
    //     for dupe_set in (&hash_dupes).iter() {
    //         let dupe_set = dupe_set.clone();
    //         fut_set.push(async {
    //             let mut exact_matches: Vec<Vec<String>> = vec!();
    //             for candidate in dupe_set {
    //                 for (i, reference) in exact_matches.iter().enumerate() {
    //                     if is_same_file(&reference[0], &candidate).unwrap_or(true) {
    //                         println!("{} and {} binary match!", reference[0], candidate);
    //                         exact_matches[i].push(candidate.to_string());
    //                         break;
    //                     }
    //                 }
    //                 exact_matches.push(vec!(candidate.to_string()));
    //             }
    //             exact_matches
    //         });
    //     }
    //     let mut fut_set = fut_set.timeout_repeating(tokio::time::interval(Duration::from_secs(5)));
    //     while let Ok(Some(result)) = fut_set.try_next().await {
    //         if cancel_token.is_cancelled() {
    //             break;
    //         }
    //         for bin_set in result {
    //             if bin_set.len() > 1 {
    //                 // println!("bin duped: {:?}", bin_set);
    //                 bin_dupes.push(bin_set);
    //             }
    //         }
    //     }
    // }

    // pub async fn cluster(&self, hash_dupes: Arc<RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<RwLock<Vec<Vec<String>>>>, hamming_range: usize, cancel_token: CancellationToken) {
    //     // Hamming range provided as 0 - 100 percentile difference.
    //     // self.find_dupes(hash_dupes.clone(), bin_dupes, cancel_token).await;
    //     let hamming_distance = (hamming_range * HASH_SIZE_BYTES * 8) / 100;
    //     let hamming_distance: u32 = hamming_distance.try_into().unwrap();
    //     println!("Checking for distance {}", hamming_distance);

    //     let mut conn = loop {
    //         if let Ok(acquisition) = self.db_pool.acquire().await {
    //             break acquisition;
    //         }
    //     };
    //     let mut hash_dupes = hash_dupes.write().await;
    //     let mut hash_dupes_phashes: Vec<image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>> = vec![];
    //     for dupe_set in (*hash_dupes).iter() {
    //         let set_phash = sqlx::query("SELECT phash FROM entries WHERE filepath = ?").bind(&dupe_set[0]).fetch_one(conn.acquire().await.unwrap()).await.unwrap();
    //         hash_dupes_phashes.push(image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>::from_base64(set_phash.get("phash")).unwrap());
    //     }
    //     assert_eq!(hash_dupes.len(), hash_dupes_phashes.len());

    //     let mut all_entries = sqlx::query("SELECT * FROM entries WHERE NOT ignored;").fetch(conn.acquire().await.unwrap());
    //     while let Some(Ok(row)) = all_entries.next().await {
    //         let mut matched = false;
    //         let phash = image_hasher::ImageHash::<[u8; HASH_SIZE_BYTES]>::from_base64(row.get("phash")).unwrap();
    //         for (idx,reference) in hash_dupes_phashes.iter().enumerate() {
    //             if phash.dist(&reference) <= hamming_distance {
    //                 hash_dupes[idx].push(row.get::<String, &str>("filepath"));
    //                 matched = true;
    //                 break;
    //             }
    //         }
    //         if !matched {
    //             hash_dupes.push(vec!(row.get::<String, &str>("filepath")));
    //             hash_dupes_phashes.push(phash);
    //             assert_eq!(hash_dupes.len(), hash_dupes_phashes.len());
    //         }
    //     }
    //     hash_dupes.retain(|x| x.len()>1);
    //     for set in hash_dupes.iter() {
    //         println!("Dupe set: {:?}", set);
    //     }
    // }
}