use eframe::{egui::{self, Id, RichText}, epaint::Color32};
use egui_extras::RetainedImage;
use tokio::runtime;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use std::{sync::{{Arc, RwLock}, atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed}}, path::PathBuf, process::exit};
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
    set_images: Vec<RetainedImage>,
    db_pool: sqlx::SqlitePool,
    dupes_started: AtomicBool,
    dupes_checked: Arc<AtomicBool>,
    hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    hamming_proximity: usize,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl Into<PathBuf>, rt: Arc<runtime::Runtime>, db_pool: sqlx::SqlitePool) -> Self {
        let mut preloaded_imgs = vec![];
        {
            let mut rd = std::fs::read_dir("./test_data").unwrap().into_iter();
            for i in 1..28 {
                while let Some(entry) = rd.next() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_file() {
                        let bytes = std::fs::read(entry.path());
                        if bytes.is_ok() {
                            if let Ok(success) = RetainedImage::from_image_bytes(format!("preload {i}"), &bytes.unwrap()) {
                                preloaded_imgs.push(success);
                                break;
                            }
                        }
                    }
                }
            }
        }
        let ig = IndexingGui {
            filelist: Arc::new(RwLock::new(None)),
            rt: Some(rt),
            hash_track: Arc::new(HashTracker::new()),
            cancel_token: CancellationToken::new(),
            checkmark: RetainedImage::from_image_bytes("checkmark", CHECKMARK).unwrap(),
            set_images: preloaded_imgs,
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
        if !path.is_dir() { eprintln!("Invalid directory \"{}\"", path.display()); exit(1)};
        // ig.rt.as_ref().unwrap().spawn(async move {
        //     Self::load_filelist(path, fl_handle.clone()).await;
        //     Self::update_filelist(fl_handle.clone(), db_pool.clone(), done_handle.clone(), cancel_handle.clone()).await.unwrap();
        //     done_handle.done_hashing.store(true, std::sync::atomic::Ordering::Relaxed);
        // });
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
        println!("Starting {} update tasks...", fut_set.len());
        hash_track.to_hash.store(fut_set.len(), Relaxed);
        tasknum = 0;
        // let _ = futures::future::join_all(fut_set).await;
        while let Some(result) = fut_set.next().await {
                    // println!("Received future {}", tasknum);
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

    fn hashing_popup(&mut self, ctx: &egui::Context) {
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
                                ui.add(egui::Slider::new(&mut self.hamming_proximity, 0..=100).text("% Difference"));
                                if ui.button(format!("Search database for images with {}% difference.", self.hamming_proximity)).clicked() {
                                    self.dupes_started.store(true, Relaxed);
                                    self.dupes_checked.store(false, Relaxed);
                                    self.hash_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
                                    self.bin_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
                                    assert_eq!(self.hash_dupes.blocking_read().len(), 0);
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

    fn deduplication_win(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            egui::containers::Frame {
                inner_margin: egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.},
                outer_margin: egui::style::Margin::default(),
                rounding: egui::Rounding::default(),
                shadow: eframe::epaint::Shadow::default(),
                fill: Color32::GRAY,
                stroke: egui::Stroke::default(),
            }.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let _ = ui.button("Deduplicate Exact Matches");
                    ui.separator();
                    ui.add(egui::widgets::DragValue::new(&mut self.hamming_proximity).clamp_range(0..=100));
                    ui.label(RichText::new("% different by hash").color(Color32::BLACK));
                    let _ = ui.button("LOAD");
                    // change to RTL
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        let _ = ui.button("Library settings");
                        ui.separator();
                        ui.label(RichText::new("0 managed images").color(Color32::BLACK));
                        let _ = ui.button("RELOAD");
                    });
                });
            });
            egui::containers::Frame {
                inner_margin: egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.},
                outer_margin: egui::style::Margin::default(),
                rounding: egui::Rounding::default(),
                shadow: eframe::epaint::Shadow::default(),
                fill: Color32::DARK_GRAY,
                stroke: egui::Stroke::new(3.0, Color32::BLACK),
            }.show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    let avail_size = ui.available_size();
                    let set_view_height = 128.;
                    // egui::ScrollArea::vertical().max_width(avail_size.x/3.).drag_to_scroll(false).show_rows(ui, set_view_height, set_views.len(), |ui, row_range| {
                    //     for set_view in &set_views[row_range] {
                    //         ui.horizontal(|ui| {
                    //             ui.label(format!("Row {}/{}", set_view, set_views.len()));
                    //             ui.image(self.set_image.texture_id(ui.ctx()), [128., 128.]);
                    //         });
                    //     }
                    // });
                    ui.vertical(|ui| {
                    egui::containers::Frame {
                        inner_margin: egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.},
                        outer_margin: egui::style::Margin::default(),
                        rounding: egui::Rounding::default(),
                        shadow: eframe::epaint::Shadow::default(),
                        fill: Color32::GRAY,
                        stroke: egui::Stroke::new(2.0, Color32::BLACK),
                    }.show(ui, |ui| {
                        egui::ScrollArea::vertical().max_width(avail_size.x/3.).drag_to_scroll(false).show_rows(ui, set_view_height, self.set_images.len(), |ui, row_range| {
                            for idx in row_range {
                                ui.allocate_ui_at_rect(egui::Rect {min: ui.cursor().min, max: ui.cursor().min+[128.,128.].into()}, |ui| {
                                    ui.horizontal_centered(|ui| {
                                        ui.label(format!("Row {}/{}", idx, self.set_images.len()));
                                        ui.image(self.set_images[idx].texture_id(ui.ctx()), aspect_fit(self.set_images[idx].size_vec2(), [128., 128.]));
                                    });
                                });
                            }
                        });
                        ui.allocate_space(egui::Vec2 {x:0., y:ui.available_height()});
                    });
                    });
                    ui.horizontal_centered(|ui| {
                        ui.image(self.checkmark.texture_id(ui.ctx()), [32.,32.]);
                    });
                    egui::containers::Frame {
                        inner_margin: egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.},
                        outer_margin: egui::style::Margin::default(),
                        rounding: egui::Rounding::default(),
                        shadow: eframe::epaint::Shadow::default(),
                        fill: Color32::GRAY,
                        stroke: egui::Stroke::new(2.0, Color32::BLACK),
                    }.show(ui, |ui| {
                        egui::ScrollArea::horizontal().max_height(140.).drag_to_scroll(false).show(ui, |ui| {
                            for i in 0..24 {
                                ui.allocate_ui_at_rect(egui::Rect {min: ui.cursor().min, max: ui.cursor().min+[128.,128.].into()}, |ui| {
                                    ui.centered_and_justified(|ui| {
                                        ui.image(self.set_images[i].texture_id(ui.ctx()), aspect_fit(self.set_images[i].size_vec2(), [128.,128.]));
                                    });
                                });
                            }
                        });
                        ui.allocate_space(ui.available_size());
                    });
                });
            });
        });
    }
}

fn aspect_fit(img_size: impl Into<egui::Vec2>, fit_size: impl Into<egui::Vec2>) -> egui::Vec2 {
    let img_size = img_size.into();
    let fit_size = fit_size.into();
    let img_rat = img_size.x / img_size.y;
    let fit_rat = fit_size.x / fit_size.y;
    let ratio;
    if img_rat < fit_rat {
        ratio = fit_size.y / img_size.y;
    } else {
        ratio = fit_size.x / img_size.x;
    }
    egui::Vec2 {x: ratio*img_size.x, y: ratio*img_size.y}
}


impl eframe::App for IndexingGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {

        egui::CentralPanel::default().show(ctx, |ui| {
            self.deduplication_win(ui);
        });
    }

    fn on_exit(&mut self, _ctx: Option<&eframe::glow::Context>) {
        self.cancel_token.cancel();
        Arc::try_unwrap(self.rt.take().unwrap()).unwrap().shutdown_background();
    }
}