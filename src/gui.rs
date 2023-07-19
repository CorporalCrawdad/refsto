use eframe::egui;
use egui_extras::RetainedImage;
use tokio::runtime;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use std::{sync::{{Arc, RwLock}, atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed}}, path::PathBuf, time::Duration};
use futures::stream::futures_unordered::FuturesUnordered;

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
    rt: Option<Arc<runtime::Runtime>>,
    hash_track: Arc<HashTracker>,
    cancel_token: CancellationToken,
    checkmark: RetainedImage,
    db_pool: sqlx::SqlitePool,
    dupes_started: AtomicBool,
    dupes_checked: Arc<AtomicBool>,
    hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    hamming_proximity: usize,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl Into<PathBuf>, rt: Arc<runtime::Runtime>, db_pool: sqlx::SqlitePool) -> Self {
        let ig = IndexingGui {
            filelist: Arc::new(RwLock::new(None)),
            rt: Some(rt),
            hash_track: Arc::new(HashTracker::new()),
            cancel_token: CancellationToken::new(),
            checkmark: RetainedImage::from_image_bytes("checkmark", CHECKMARK).unwrap(),
            db_pool: db_pool.clone(),
            hash_dupes: Arc::new(tokio::sync::RwLock::new(vec!())),
            bin_dupes: Arc::new(tokio::sync::RwLock::new(vec!())),
            dupes_started: AtomicBool::new(false),
            dupes_checked: Arc::new(AtomicBool::new(false)),
            hamming_proximity: 0,
        };
        let fl_handle = ig.filelist.clone();
        let done_handle = ig.hash_track.clone();
        let cancel_handle = ig.cancel_token.clone();
        let path = path.into();
        if !path.is_dir() { panic!("Invalid directory {}", path.display())};
        ig.rt.as_ref().unwrap().spawn(async move {
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
        let mut tasknum = 0;
        // let mut fut_set = vec!();
        let mut fut_set = FuturesUnordered::new();
        {
            if let Ok(lock) = filelist.read() {
                if let Some(filelist) = &*lock {
                    for entry in filelist {
                        let entry = entry.to_str().unwrap();
                        fut_set.push(hi.update(String::from(entry), tasknum));
                        tasknum += 1;
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
        tasknum = 0;
        // let _ = futures::future::join_all(fut_set).await;
        while let Some(result) = fut_set.next().await {
                    println!("Received future {}", tasknum);
                    tasknum += 1;
                    hash_track.hashed.fetch_add(1, Relaxed);
                    if cancel_token.is_cancelled() {
                        return Err(anyhow::anyhow!("hashing futures cancelled before completion"));
                    } else {
                        match result {
                            Ok(_) => {},
                            Err(HashIndexError::Format) => {hash_track.bad_format.fetch_add(1, Relaxed);},
                            Err(HashIndexError::Encoding) => {hash_track.bad_encode.fetch_add(1, Relaxed);},
                            Err(_) => (),
                        }
                    }
        }
        println!("DB updates completed.");
        Ok(())
    }

    fn spawn_find_dupes(rt_handle: &runtime::Handle, cancel_token: CancellationToken, db_pool: sqlx::SqlitePool, hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, hamming_proximity: usize, done_flag: Arc<AtomicBool>) {
        rt_handle.spawn(async move {
            let hi = HashIndexer::new(db_pool);
            hi.cluster(hash_dupes, bin_dupes, hamming_proximity, cancel_token).await;
            done_flag.store(true, Relaxed);
        });
    }
}

impl eframe::App for IndexingGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::Window::new("Updating image hash database...").show(ctx, |ui| {
            match self.filelist.try_read() {
                Ok(mutguard) => {
                    if let Some(filelist) = &*mutguard {
                        ui.horizontal(|ui| {
                            ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                            ui.label(format!("File list loaded! Found {} files...", filelist.len()));
                        });
                        if self.hash_track.done_hashing.load(Relaxed) {
                            ui.horizontal(|ui| {
                                ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                                ui.label(format!("Hashing complete for {} files", self.hash_track.hashed.load(Relaxed)));
                            });
                            if self.dupes_checked.load(Relaxed) {
                                match (self.hash_dupes.try_read(), self.bin_dupes.try_read()) {
                                    (Ok(hash_dupes), Ok(bin_dupes)) => {
                                        ui.horizontal(|ui| {
                                            self.dupes_started.store(false, Relaxed);
                                            ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                                            ui.label(format!("Found {} sets of images with at least one duplication.", hash_dupes.len()+bin_dupes.len()));
                                        });
                                        for set in hash_dupes.iter().chain(bin_dupes.iter()) {
                                            ui.label(format!("DUPLICATE SET: (len {})\t{:?}", set.len(), set.join(", ")));
                                        }
                                    },
                                    (_,_) => {
                                        ui.horizontal(|ui| {
                                            ui.add(egui::Spinner::new());
                                            ui.add(egui::Label::new("Comparing for binary duplicates and visually identical images..."));
                                        });
                                    },
                                }
                            }
                            if !self.dupes_started.load(Relaxed) {
                                ui.add(egui::Slider::new(&mut self.hamming_proximity, 0..=100).text("Similarity"));
                                if ui.button(format!("Search database for images with {}% difference.", self.hamming_proximity)).clicked() {
                                    self.dupes_started.store(true, Relaxed);
                                    self.dupes_checked.store(false, Relaxed);
                                    self.hash_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
                                    self.bin_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
                                    Self::spawn_find_dupes(self.rt.as_ref().unwrap().handle(), self.cancel_token.clone(), self.db_pool.clone(), self.hash_dupes.clone(), self.bin_dupes.clone(), self.hamming_proximity, self.dupes_checked.clone());
                                }
                            }
                        } else {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(format!("Hashing files... {}/{}", self.hash_track.hashed.load(Relaxed), self.hash_track.to_hash.load(Relaxed)));
                                if self.hash_track.bad_encode.load(Relaxed) > 0 {
                                    ui.label(format!("BAD ENCODED FILES ENCOUNTERED! Malformed pictures cannot be hashed, try fixing with imagemagick and re-run."));
                                }
                            });
                        }
                    } else {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new());
                            ui.add(egui::Label::new("Loading filelist..."));
                        });
                    }
                },
                Err(std::sync::TryLockError::WouldBlock) => {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new());
                        ui.add(egui::Label::new("Loading filelist..."));
                    });
                },
                Err(_) => panic!(),
            }
        });
    }

    fn on_exit(&mut self, _ctx: Option<&eframe::glow::Context>) {
        self.cancel_token.cancel();
        Arc::try_unwrap(self.rt.take().unwrap()).unwrap().shutdown_background();
    }
}