use eframe::egui;
use egui_extras::RetainedImage;
use tokio::runtime;
use tokio_util::sync::CancellationToken;
use std::{sync::{{Arc, RwLock}, atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed}}, path::PathBuf};
use futures::{stream::futures_unordered::FuturesUnordered, StreamExt};

use crate::index::{HashIndexer, HashIndexError};

const CHECKMARK: &[u8] = include_bytes!("../assets/checkmark.png");

struct HashTracker {
    done_hashing: AtomicBool,
    to_hash: AtomicUsize,
    hashed: AtomicUsize,
    bad_format: AtomicUsize,
    bad_encode: AtomicUsize,
}

impl HashTracker {
    fn new() -> Self {
        HashTracker{done_hashing: AtomicBool::new(false), to_hash: AtomicUsize::new(0), hashed: AtomicUsize::new(0), bad_format: AtomicUsize::new(0), bad_encode: AtomicUsize::new(0)}
    }
}

pub struct IndexingGui {
    filelist: Arc<RwLock<Option<Vec<PathBuf>>>>,
    rt: Box<Arc<runtime::Runtime>>,
    hash_track: Arc<HashTracker>,
    cancel_token: CancellationToken,
    checkmark: RetainedImage,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl Into<PathBuf>, rt: Box<Arc<runtime::Runtime>>, db_pool: sqlx::SqlitePool) -> Self {
        let ig = IndexingGui {
            filelist: Arc::new(RwLock::new(None)),
            rt,
            hash_track: Arc::new(HashTracker::new()),
            cancel_token: CancellationToken::new(),
            checkmark: RetainedImage::from_image_bytes("checkmark", CHECKMARK).unwrap(),
        };
        let fl_handle = ig.filelist.clone();
        let done_handle = ig.hash_track.clone();
        let cancel_handle = ig.cancel_token.clone();
        let path = path.into();
        ig.rt.spawn(async move {
            Self::load_filelist(path, fl_handle.clone()).await;
            Self::update_filelist(fl_handle.clone(), db_pool.clone(), done_handle.clone(), cancel_handle.clone()).await.unwrap();
            done_handle.done_hashing.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        ig
    }

    async fn load_filelist(path: impl Into<PathBuf> + std::marker::Send, filelist: Arc<RwLock<Option<Vec<PathBuf>>>>) {
        let path: PathBuf = path.into();
        if path.is_dir() {
            let mut dirlist: _ = vec![path.into()];
            let mut found_files: _ = vec![];
            while let Some(dir_to_check) = dirlist.pop() {
                match std::fs::read_dir(dir_to_check) {
                    Ok(readdir) => {
                        for entry in readdir {
                            if let Ok(entry) = entry {
                                if let Ok(ft) = entry.file_type() {
                                    if ft.is_dir() {
                                        dirlist.push(entry.path());
                                    } else if ft.is_file() {
                                        found_files.push(entry.path());
                                    }
                                }
                            }
                        }
                    },
                    Err(error) => {
                        if found_files.len() == 0 {
                            panic!("{}", error);
                        } else {
                            eprintln!("{}", error);
                        }
                    },
                }
            }
            *filelist.write().unwrap() = Some(found_files);
        }
    }

    async fn update_filelist(filelist: Arc<RwLock<Option<Vec<PathBuf>>>>, db_pool: sqlx::SqlitePool, hash_track: Arc<HashTracker>, cancel_token: CancellationToken) -> Result<(), anyhow::Error> {
        let hi = HashIndexer::new(db_pool);
        // let mut fut_set = vec!();
        let mut fut_set = FuturesUnordered::new();
        {
            let lock = filelist.read();
            if let Ok(lock) = lock {
                if let Some(filelist) = &*lock {
                    for entry in filelist {
                        let entry = entry.to_str().unwrap();
                        fut_set.push(hi.update(String::from(entry)));
                    }
                }
            } else {
                println!("Couldn't lock filelist!");
            }
        }
        // for future in fut_set {
        //     if let Err(e) = future.await{
        //         eprintln!("{}", e);
        //     }
        // }
        println!("Starting {} update tasks...", fut_set.len());
        hash_track.to_hash.store(fut_set.len(), Relaxed);
        // let _ = futures::future::join_all(fut_set).await;
        while let Some(result) = fut_set.next().await {
            if cancel_token.is_cancelled() {
                return Err(anyhow::anyhow!("hashing futures cancelled before completion"));
            } else {
                match result {
                    Ok(_) => {hash_track.hashed.fetch_add(1, Relaxed);},
                    Err(HashIndexError::Format) => {hash_track.bad_format.fetch_add(1, Relaxed);},
                    Err(HashIndexError::Encoding) => {hash_track.bad_encode.fetch_add(1, Relaxed);},
                    Err(_) => (),
                }
            }
        }
        println!("DB updates completed.");
        Ok(())
    }
}

impl eframe::App for IndexingGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let lock = self.filelist.try_read();
            match lock {
                Ok(mutguard) => {
                    if let Some(filelist) = &*mutguard {
                        ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                        ui.label(format!("File list loaded! Found {} files...", filelist.len()));
                        if self.hash_track.done_hashing.load(Relaxed) {
                            ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                            ui.label(format!("Hashing complete for {} files", self.hash_track.hashed.load(Relaxed)));
                        } else {
                            ui.spinner();
                            ui.label(format!("Hashing files... {}/{}", self.hash_track.hashed.load(Relaxed), self.hash_track.to_hash.load(Relaxed)));
                            if self.hash_track.bad_encode.load(Relaxed) > 0 {
                                ui.label(format!("BAD ENCODED FILES ENCOUNTERED! Malformed pictures cannot be hashed, try fixing with imagemagick and re-run."));
                            }
                        }
                    } else {
                        ui.add(egui::Spinner::new());
                        ui.add(egui::Label::new("Loading filelist..."));
                    }
                },
                Err(std::sync::TryLockError::WouldBlock) => {
                    ui.add(egui::Spinner::new());
                    ui.add(egui::Label::new("Loading filelist..."));
                },
                Err(_) => panic!(),
            }
        });
    }

    fn on_exit(&mut self, _ctx: Option<&eframe::glow::Context>) {
        self.cancel_token.cancel();
    }
}