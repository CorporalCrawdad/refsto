// use eframe::{egui::{self, RichText}, epaint::Color32};
use eframe::{egui, egui::{RichText, Color32, Vec2, Rect}};
use egui_extras::RetainedImage;
use tokio::runtime;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use std::{sync::{{Arc, RwLock}, atomic::{AtomicBool, Ordering::Relaxed}, mpsc, mpsc::TryRecvError}, path::PathBuf, collections::HashSet};
use futures::stream::futures_unordered::FuturesUnordered;
use sqlx::{Row,Acquire};

use crate::index::{HashIndexer, HashIndexError};

const CHECKMARK: &[u8] = include_bytes!("../assets/checkmark.png");

pub struct IndexingGui {
    watched_dirs: Arc<RwLock<HashSet<PathBuf>>>,
    rt: Option<Arc<runtime::Runtime>>,
    cancel_token: CancellationToken,
    hashing_cancelled: CancellationToken,
    hashing_complete: bool,
    checkmark: RetainedImage,
    set_images: Vec<RetainedImage>,
    set_images_recv: mpsc::Receiver<RetainedImage>,
    filelist_recv: Option<mpsc::Receiver<PathBuf>>,
    db_pool: sqlx::SqlitePool,
    _dupes_started: AtomicBool,
    _dupes_checked: Arc<AtomicBool>,
    _hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    _bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>,
    hamming_proximity: usize,
    show_library_man: bool,
    show_dialog_err: bool,
    show_hashing: bool,
    filelist: HashSet<PathBuf>,
    filelist_loaded: bool,
    rehashed_cnt: usize,
    rehashed_cnt_recv: Option<mpsc::Receiver<usize>>,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, rt: Arc<runtime::Runtime>, db_pool: sqlx::SqlitePool) -> Self {
        let (set_images_tx, set_images_rx) = std::sync::mpsc::channel();
        let mut ig = IndexingGui {
            watched_dirs: Arc::new(RwLock::new(HashSet::new())),
            rt: Some(rt),
            cancel_token: CancellationToken::new(),
            hashing_cancelled: CancellationToken::new(),
            checkmark: RetainedImage::from_image_bytes("checkmark", CHECKMARK).unwrap(),
            set_images: vec![],
            set_images_recv: set_images_rx,
            db_pool: db_pool.clone(),
            _hash_dupes: Arc::new(tokio::sync::RwLock::new(vec!())),
            _bin_dupes: Arc::new(tokio::sync::RwLock::new(vec!())),
            _dupes_started: AtomicBool::new(false),
            _dupes_checked: Arc::new(AtomicBool::new(false)),
            hamming_proximity: 0,
            show_library_man: false,
            show_dialog_err: false,
            filelist: HashSet::new(),
            filelist_recv: None,
            filelist_loaded: false,
            show_hashing: false,
            hashing_complete: true,
            rehashed_cnt: 0,
            rehashed_cnt_recv: None,
        };

        ig.rt.as_ref().unwrap().spawn(async move {
            let mut fut_set = FuturesUnordered::new();
            let mut rd_iter = std::fs::read_dir("./test_data").unwrap();
            while let Some(Ok(entry)) = rd_iter.next() {
                if let Ok(ftype) = entry.file_type() {
                    if ftype.is_file() {
                        fut_set.push(async move {
                            if let Ok(success) = image::load_from_memory(&std::fs::read(entry.path()).unwrap()) {
                                let image = success.thumbnail(128, 128);
                                let color_image = egui::ColorImage::from_rgba_unmultiplied([image.width().try_into().unwrap(), image.height().try_into().unwrap()], image.to_rgba8().as_flat_samples().as_slice());
                                return Some(RetainedImage::from_color_image(entry.file_name().to_string_lossy(), color_image))
                            } else {
                                eprintln!("Resize failed on image {}...", entry.path().to_string_lossy());
                                return None
                            }
                        });
                    }
                }
            }
            while let Some(ri) = fut_set.next().await {
                if let Some(ri) = ri {
                    let _ = set_images_tx.send(ri);
                }
            }
        });
        ig.get_watched_dirs();
        ig
    }

    fn get_watched_dirs(&mut self) {
        println!("Loading watched_dirs from database...");
        let db_pool = self.db_pool.clone();
        let wd_lock = self.watched_dirs.clone();
        self.rt.as_ref().unwrap().spawn(async move {
            let mut conn = loop {
                if let Ok(acquisition) = db_pool.acquire().await {
                    break acquisition;
                }
            };
            if let Ok(new_watched_dirs) = sqlx::query("SELECT fullpath FROM watched_dirs").fetch_all(conn.acquire().await.unwrap()).await {
                println!("query result len: {}", new_watched_dirs.len());
                if let Ok(mut wd_lock) = wd_lock.write() {
                    *wd_lock = HashSet::new();
                    for row in new_watched_dirs {
                        println!("Adding {} to watched_dirs from database", row.get::<&str,&str>("fullpath"));
                        (*wd_lock).insert(PathBuf::from(row.get::<&str,&str>("fullpath")));
                    }
                }
            }
            println!("watched_dirs loaded from database");
        });
    }

    fn add_watched_dir(&mut self, dir: PathBuf) {
        if dir.is_dir() && dir.is_absolute() && !(&self.watched_dirs.read().unwrap()).iter().any(|x| {x == &dir}) {
            let db_pool = self.db_pool.clone();
            let wd_lock = self.watched_dirs.clone();
            self.rt.as_ref().unwrap().spawn(async move {
                let mut conn = loop {
                    if let Ok(acquisition) = db_pool.acquire().await {
                        break acquisition;
                    }
                };
                if sqlx::query("INSERT INTO watched_dirs (fullpath) VALUES (?)").bind(dir.to_str()).execute(conn.acquire().await.unwrap()).await.is_ok() {
                    if let Ok(mut wd_lock) = wd_lock.write() {
                        (*wd_lock).insert(dir);
                    }
                }
            });
        }
    }

    fn del_watched_dir(&mut self, dir: PathBuf, purge_imgs: bool) {
        if (&self.watched_dirs.read().unwrap()).iter().any(|x| {x == dir.as_os_str()}) {
            let db_pool = self.db_pool.clone();
            let wd_lock = self.watched_dirs.clone();
            self.rt.as_ref().unwrap().spawn(async move {
                let mut conn = loop {
                    if let Ok(acquisition) = db_pool.acquire().await {
                        break acquisition;
                    }
                };
                if sqlx::query("DELETE FROM watched_dirs WHERE fullpath=?").bind(dir.to_str().unwrap()).execute(conn.acquire().await.unwrap()).await.is_ok() {
                    if let Ok(mut wd_lock) = wd_lock.write() {
                        wd_lock.remove::<PathBuf>(&dir);
                        if purge_imgs {
                            todo!("remove all images from database covered by {} and not covered by any other watched_dirs entries", dir.to_string_lossy());
                        }
                    }
                }
            });
        }
    }

    fn get_set_images(&mut self) -> &Vec<RetainedImage> {
        while let Ok(rx) = self.set_images_recv.try_recv() {
            self.set_images.push(rx);
        };
        &self.set_images
    }

    // async fn load_filelist(paths: Arc<RwLock<HashSet<PathBuf>>>) -> Vec<PathBuf> {
    //     let paths = paths.read().unwrap();
    //     let mut filelist = vec![];
    //     for path in (*paths).iter() {
    //         if path.is_dir() {
    //             let mut dirlist: _ = vec![path.into()];
    //             let mut found_files: _ = vec![];
    //             while let Some(dir_to_check) = dirlist.pop() {
    //                 match std::fs::read_dir(dir_to_check) {
    //                     Ok(readdir) => {
    //                         for entry in readdir {
    //                             if let Ok(entry) = entry {
    //                                 if let Ok(ft) = entry.file_type() {
    //                                     if ft.is_dir() {
    //                                         dirlist.push(entry.path());
    //                                     } else if ft.is_file() {
    //                                         found_files.push(entry.path());
    //                                     }
    //                                 }
    //                             }
    //                         }
    //                     },
    //                     Err(error) => {
    //                         if found_files.len() == 0 {
    //                             panic!("{}", error);
    //                         } else {
    //                             eprintln!("{}", error);
    //                         }
    //                     },
    //                 }
    //             }
    //             filelist.append(&mut found_files);
    //         }
    //     }
    //     filelist
    // }

    // async fn update_from_watched_dirs(watched_dirs: Arc<RwLock<Vec<PathBuf>>>, filelist: Arc<RwLock<Option<Vec<PathBuf>>>>, db_pool: sqlx::SqlitePool, hash_track: Arc<HashTracker>, cancel_token: CancellationToken) -> Result<(), anyhow::Error> {
    //     let hi = HashIndexer::new(db_pool);
    //     let mut tasknum = 0;
    //     // let mut fut_set = vec!();
    //     let mut fut_set = FuturesUnordered::new();
    //     {
    //         if let Ok(lock) = filelist.read() {
    //             if let Some(filelist) = &*lock {
    //                 for entry in filelist {
    //                     let entry = entry.to_str().unwrap();
    //                     fut_set.push(hi.update(String::from(entry), tasknum));
    //                     tasknum += 1;
    //                 }
    //             }
    //         } else {
    //             eprintln!("Couldn't lock filelist!");
    //         }
    //     }
    //     println!("Starting {} update tasks...", fut_set.len());
    //     hash_track.to_hash.store(fut_set.len(), Relaxed);
    //     tasknum = 0;
    //     // let _ = futures::future::join_all(fut_set).await;
    //     while let Some(result) = fut_set.next().await {
    //                 tasknum += 1;
    //                 hash_track.hashed.fetch_add(1, Relaxed);
    //                 if cancel_token.is_cancelled() {
    //                     return Err(anyhow::anyhow!("hashing futures cancelled before completion"));
    //                 } else {
    //                     match result {
    //                         Ok(_) => {},
    //                         Err(HashIndexError::Format) => {hash_track.bad_format.fetch_add(1, Relaxed);},
    //                         Err(HashIndexError::Encoding) => {hash_track.bad_encode.fetch_add(1, Relaxed);},
    //                         Err(_) => (),
    //                     }
    //                 }
    //     }
    //     println!("DB updates completed.");
    //     Ok(())
    // }

    fn spawn_find_dupes(rt_handle: &runtime::Handle, cancel_token: CancellationToken, db_pool: sqlx::SqlitePool, hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, hamming_proximity: usize, done_flag: Arc<AtomicBool>) {
        rt_handle.spawn(async move {
            let hi = HashIndexer::new(db_pool);
            hi.cluster(hash_dupes, bin_dupes, hamming_proximity, cancel_token).await;
            done_flag.store(true, Relaxed);
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
                    if ui.button("LOAD").clicked() {
                    }
                    // change to RTL
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.button("Library settings").clicked() {
                            self.show_library_man = true;
                            self.filelist_loaded = false;
                        }
                        ui.separator();
                        ui.label(RichText::new("0 managed images").color(Color32::BLACK));
                        if ui.button("RELOAD").clicked() {
                            self.show_hashing = true;
                            self.filelist_loaded = false;
                            self.hashing_cancelled = CancellationToken::new();
                        }
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
                    ui.vertical(|ui| {
                    egui::containers::Frame {
                        inner_margin: egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.},
                        outer_margin: egui::style::Margin::default(),
                        rounding: egui::Rounding::default(),
                        shadow: eframe::epaint::Shadow::default(),
                        fill: Color32::GRAY,
                        stroke: egui::Stroke::new(2.0, Color32::BLACK),
                    }.show(ui, |ui| {
                        let snapshot_set_images_len = self.get_set_images().len();
                        egui::ScrollArea::vertical().max_width(avail_size.x/3.).drag_to_scroll(false).show_rows(ui, 128., snapshot_set_images_len, |ui, row_range| {
                            for idx in row_range {
                                ui.allocate_ui_at_rect(Rect {min: ui.cursor().min, max: ui.cursor().min+[128.,128.].into()}, |ui| {
                                    ui.horizontal_centered(|ui| {
                                        ui.label(format!("Row {}/{}", idx, snapshot_set_images_len));
                                        let cursor = Rect::from_min_max(ui.cursor().min, ui.cursor().min+[128.,128.].into());
                                        ui.allocate_ui_at_rect(Rect {min: ui.cursor().min, max: ui.cursor().min+[128.,128.].into()}, |ui| {
                                            ui.centered_and_justified(|ui| {
                                                ui.image(self.get_set_images()[idx].texture_id(ui.ctx()), aspect_fit(self.get_set_images()[idx].size_vec2(), [128., 128.]));
                                            });
                                        });
                                        ui.painter_at(cursor).add(egui::Shape::rect_stroke(Rect {min: cursor.min, max: cursor.min+[128.,128.].into()}, egui::Rounding::none(), egui::Stroke::new(5., egui::Color32::from_rgb(255, 100, 100))));
                                    });
                                });
                            }
                        });
                        ui.allocate_space(Vec2 {x:0., y:ui.available_height()});
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
                            for img in self.get_set_images().iter() {
                                ui.allocate_ui_at_rect(Rect {min: ui.cursor().min, max: ui.cursor().min+[128.,128.].into()}, |ui| {
                                    ui.centered_and_justified(|ui| {
                                        ui.image(img.texture_id(ui.ctx()), aspect_fit(img.size_vec2(), [128.,128.]));
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

    fn watch_dir_manager_win(&mut self, ctx: &egui::Context) {
        let mut dir_to_del: Option<PathBuf> = None;
        let mut dir_to_add = None;
        if self.show_library_man { egui::Area::new("dirwatchwin").movable(false).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_CENTER, [0.,0.]).show(ctx, |ui| {
        // egui::Window::new("dirwatchwin").movable(false).anchor(egui::Align2::CENTER_CENTER, [0.,0.]).collapsible(false).resizable(false).open(&mut self.show_library_man).show(ctx, |ui| {
            egui::containers::Frame::popup(&egui::Style::default())
                .inner_margin(egui::style::Margin { left: 10., right: 10., top: 4., bottom: 4.})
                .fill(Color32::GRAY)
                // .outer_margin(egui::style::Margin::default()) .rounding(egui::Rounding::default()) .stroke(egui::Stroke::default())
            .show(ui, |ui| {
                ui.set_max_size([400.,600.,].into());
                // ui.horizontal(|ui| {
                //     egui::containers::TopBottomPanel::top("dir manager win panel").show_inside(ui, |ui| {
                //         ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                //             self.show_library_man = !ui.add(egui::Button::new(RichText::new("🗙").color(Color32::WHITE).strong().size(20.)).fill(Color32::LIGHT_RED)).clicked();
                //         });
                //     });
                // });
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Drag and drop directories to add or").color(egui::Color32::BLACK));
                    ui.add_space(18.);
                    if ui.button(RichText::new("Add a Directory").color(egui::Color32::from_rgb(0, 255, 255))).clicked() {
                        if let Ok(dlg_result) = native_dialog::FileDialog::new()
                            .set_location("~")
                            .show_open_single_dir() {
                                dir_to_add = dlg_result;
                            } else {
                                self.show_dialog_err = true;
                            }
                    };
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                            self.show_library_man = !ui.add(egui::Button::new(RichText::new("🗙").color(Color32::WHITE).strong().size(20.)).fill(Color32::LIGHT_RED)).clicked();
                    });
                });
                egui::ScrollArea::vertical().show(ui, |ui| {
                    match self.watched_dirs.try_read() {
                        Ok(watched_dirs) => {
                            for dir in (*watched_dirs).iter() {
                                egui::containers::Frame {
                                    inner_margin: egui::style::Margin { left: 2., right: 2., top: 4., bottom: 4.},
                                    outer_margin: egui::style::Margin { left: 2., right: 2., top: 4., bottom: 4.},
                                    rounding: egui::Rounding::default(),
                                    shadow: eframe::epaint::Shadow::default(),
                                    fill: Color32::from_rgb(224, 252, 250),
                                    stroke: egui::Stroke::default(),
                                }.show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        let entrycolor; 
                                        if dir.is_dir() {
                                            entrycolor = Color32::BLACK;
                                        } else {
                                            entrycolor = Color32::DARK_RED;
                                            ui.label(RichText::new("!!!").color(entrycolor));
                                        };
                                        ui.label(RichText::new(dir.to_string_lossy()).color(entrycolor));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                                            if ui.add(egui::Button::new(RichText::new("REMOVE").strong().color(Color32::WHITE)).fill(egui::Color32::RED)).clicked() {
                                                dir_to_del = Some(dir.to_owned());
                                            };
                                        });
                                    });
                                });
                            }
                        },
                        Err(_) => {
                            ui.horizontal(|ui| {
                                ui.label("Loading managed directories from database...");
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                                    ui.spinner();
                                });
                            });
                        },
                    }
                });
            ui.allocate_space(ui.available_size());
            });
        });}
        if let Some(dir) = dir_to_del {
            self.del_watched_dir(dir, false);
        }
        if let Some(dir) = dir_to_add {
            self.add_watched_dir(dir);
        }
        for file in ctx.input(|i| { i.raw.dropped_files.clone() }) {
            match file.path {
                Some(path) => {
                    if path.is_dir() {
                        self.add_watched_dir(path);
                    } else {
                        eprintln!("Dropped file {} is not a directory, skipping...", path.to_string_lossy());
                    }

                },
                None => eprintln!("path not set on dropped directory {}", file.name),
            }
        }
    }

    // fn hashing_popup(&mut self, ctx: &egui::Context) {
    //     egui::Window::new("Updating image hash database...").show(ctx, |ui| { 
    //         match self.filelist.try_read() {
    //             Ok(mutguard) => {
    //                 if let Some(filelist) = &*mutguard {
    //                     ui.horizontal(|ui| {
    //                         ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
    //                         ui.label(format!("File list loaded! Found {} files...", filelist.len()));
    //                     });
    //                     if self.hash_track.done_hashing.load(Relaxed) {
    //                         ui.horizontal(|ui| {
    //                             ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
    //                             ui.label(format!("Hashing complete for {} files", self.hash_track.hashed.load(Relaxed)));
    //                         });
    //                         if self.dupes_checked.load(Relaxed) {
    //                             match (self.hash_dupes.try_read(), self.bin_dupes.try_read()) {
    //                                 (Ok(hash_dupes), Ok(bin_dupes)) => {
    //                                     ui.horizontal(|ui| {
    //                                         self.dupes_started.store(false, Relaxed);
    //                                         ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
    //                                         ui.label(format!("Found {} sets of images with at least one duplication.", hash_dupes.len()+bin_dupes.len()));
    //                                     });
    //                                     for set in hash_dupes.iter().chain(bin_dupes.iter()) {
    //                                         ui.label(format!("DUPLICATE SET: (len {})\t{:?}", set.len(), set.join(", ")));
    //                                     }
    //                                 },
    //                                 (_,_) => {
    //                                     ui.horizontal(|ui| {
    //                                         ui.add(egui::Spinner::new());
    //                                         ui.add(egui::Label::new("Comparing for binary duplicates and visually identical images..."));
    //                                     });
    //                                 },
    //                             }
    //                         }
    //                         if !self.dupes_started.load(Relaxed) {
    //                             ui.add(egui::Slider::new(&mut self.hamming_proximity, 0..=100).text("% Difference"));
    //                             if ui.button(format!("Search database for images with {}% difference.", self.hamming_proximity)).clicked() {
    //                                 self.dupes_started.store(true, Relaxed);
    //                                 self.dupes_checked.store(false, Relaxed);
    //                                 self.hash_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
    //                                 self.bin_dupes = Arc::new(tokio::sync::RwLock::new(vec!()));
    //                                 assert_eq!(self.hash_dupes.blocking_read().len(), 0);
    //                                 Self::spawn_find_dupes(self.rt.as_ref().unwrap().handle(), self.cancel_token.clone(), self.db_pool.clone(), self.hash_dupes.clone(), self.bin_dupes.clone(), self.hamming_proximity, self.dupes_checked.clone());
    //                             }
    //                         }
    //                     } else {
    //                         ui.horizontal(|ui| {
    //                             ui.spinner();
    //                             ui.label(format!("Hashing files... {}/{}", self.hash_track.hashed.load(Relaxed), self.hash_track.to_hash.load(Relaxed)));
    //                             if self.hash_track.bad_encode.load(Relaxed) > 0 {
    //                                 ui.label(format!("BAD ENCODED FILES ENCOUNTERED! Malformed pictures cannot be hashed, try fixing with imagemagick and re-run."));
    //                             }
    //                         });
    //                     }
    //                 } else {
    //                     ui.horizontal(|ui| {
    //                         ui.add(egui::Spinner::new());
    //                         ui.add(egui::Label::new("Loading filelist..."));
    //                     });
    //                 }
    //             },
    //             Err(std::sync::TryLockError::WouldBlock) => {
    //                 ui.horizontal(|ui| {
    //                     ui.add(egui::Spinner::new());
    //                     ui.add(egui::Label::new("Loading filelist..."));
    //                 });
    //             },
    //             Err(_) => panic!(),
    //         }
    //     });
    // }

    fn hashing_progress_win(&mut self, ctx: &egui::Context) {
        if self.show_hashing {
            egui::Area::new("Hashing Images...").movable(false).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_CENTER, [0.,0.]).show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::GRAY)
                    .stroke(egui::Stroke::new(3., egui::Color32::BLACK))
                    .rounding(egui::Rounding::same(4.))
                    .inner_margin(egui::Margin::symmetric(3., 3.))
                    .show(ui, |ui| {
                        if !self.filelist_loaded { match &self.filelist_recv {
                            Some(filelist_recv) => {
                                loop {
                                    match filelist_recv.try_recv() {
                                        Ok(filename) => {self.filelist.insert(filename);},
                                        Err(TryRecvError::Empty) => break,
                                        Err(TryRecvError::Disconnected) => {
                                            self.filelist_loaded = true;
                                            self.hashing_complete = false;
                                            let mut fut_set = FuturesUnordered::new();
                                            self.rehashed_cnt = 0;
                                            let (tx, rx) = mpsc::channel();
                                            self.rehashed_cnt_recv = Some(rx);
                                            for entry in &self.filelist {
                                                println!("looking at {}", entry.to_string_lossy());
                                                let db_pool = self.db_pool.clone();
                                                let entry = entry.to_owned();
                                                let tx = tx.clone();
                                                fut_set.push(async move {
                                                    match HashIndexer::new(db_pool).update(entry.to_string_lossy().into()).await {
                                                        Ok(_) => (),
                                                        Err(HashIndexError::Encoding) => eprintln!("Encoding Error on file '{}'", entry.to_string_lossy()),
                                                        // Enable for verbose skipping of non-image files
                                                        // Err(HashIndexError::Format) => eprintln!("Skipping bad format file '{}'", entry.to_string_lossy()),
                                                        Err(HashIndexError::Format) => (),
                                                        Err(HashIndexError::MalformedDB) => eprintln!("MalformedDB Error on file '{}'", entry.to_string_lossy()),
                                                        Err(HashIndexError::InsertDB) => eprintln!("InsertDB Error on file '{}'", entry.to_string_lossy()),
                                                        Err(HashIndexError::Other) => eprintln!("Other Error on file '{}'", entry.to_string_lossy()),
                                                    }
                                                    tx.send(1).unwrap();
                                                })
                                            }
                                            let ct = self.hashing_cancelled.clone();
                                            println!("started {} update tasks", fut_set.len());
                                            self.rt.as_ref().unwrap().spawn(async move {
                                                while let Some(_) = fut_set.next().await { if ct.is_cancelled() { break };}
                                            });
                                            break
                                        }
                                    }
                                }
                                while let Ok(filename) = filelist_recv.try_recv() {
                                    self.filelist.insert(filename);
                                }
                            },
                            None => {
                                if let Ok(lock) = self.watched_dirs.read() {
                                    let (tx,rx) = std::sync::mpsc::channel::<PathBuf>();
                                    self.filelist_recv = Some(rx);
                                    let mut fut_set = FuturesUnordered::new();
                                    for dir in lock.iter() {
                                        if !dir.is_dir() {
                                            eprintln!("{} is not a directory!!", dir.to_string_lossy());
                                            continue
                                        }
                                        let tx = tx.clone();
                                        let dir = dir.to_owned();
                                        println!("Adding future for dir {}", dir.to_string_lossy());
                                        fut_set.push(async move {
                                            let mut dirlist = vec![dir];
                                            while let Some(dir) = dirlist.pop() {
                                                if let Ok(entries) = dir.read_dir() {
                                                    for entry in entries {
                                                        if let Ok(entry) = entry {
                                                            let ft = entry.file_type().unwrap();
                                                            if ft.is_file() {
                                                                if tx.send(entry.path()).is_err() {
                                                                    eprintln!("Error: mpsc closed before receiving {}", entry.path().to_string_lossy());
                                                                }
                                                            } else if ft.is_dir() {
                                                                dirlist.push(entry.path());
                                                            } else {
                                                                eprintln!("Can't interpret filetype of: {}", entry.path().to_string_lossy());
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        })
                                    }
                                    let ct = self.hashing_cancelled.clone();
                                    self.rt.as_ref().unwrap().spawn(async move {
                                        while let Some(_) = fut_set.next().await{ if ct.is_cancelled() { break } }
                                    });
                                }
                            }
                        } } else {
                            self.filelist_recv = None;
                            if let Some(recv) = &self.rehashed_cnt_recv {
                                match recv.try_recv() {
                                    Ok(rx) => self.rehashed_cnt += rx,
                                    Err(TryRecvError::Empty) => (),
                                    Err(TryRecvError::Disconnected) => {self.hashing_complete = true},
                                };
                            }
                        } 
                        ui.horizontal(|ui| {
                            if self.filelist_loaded {
                                ui.image(self.checkmark.texture_id(ctx), [12.,12.]);
                            } else {
                                ui.add(egui::Spinner::new().size(24.));
                            }
                            ui.label(egui::RichText::new(format!("Discovered {} files...", self.filelist.len())).color(egui::Color32::BLACK));
                        });
                        // let widest = ui.horizontal(|ui| {
                        ui.horizontal(|ui| {
                            match (self.hashing_complete, self.filelist_loaded) {
                                (true,true) => {ui.image(self.checkmark.texture_id(ctx), [12.,12.]); self.rehashed_cnt_recv = None},
                                (false,true) => {ui.add(egui::Spinner::new().size(12.));},
                                (_, false) => ui.add_space(12.),
                            }
                            ui.label(egui::RichText::new(format!("Updated {}/{} files...", self.rehashed_cnt, self.filelist.len())).color(egui::Color32::BLACK));
                        }).response.rect.width();
                        if self.hashing_complete && self.filelist_loaded {
                            ui.horizontal(|ui| { ui.label(egui::RichText::new("Hashing complete, database updated!").color(egui::Color32::LIGHT_GREEN));});
                        }
                        // ui.allocate_ui_with_layout([widest, 0.].into(), egui::Layout::top_down(egui::Align::Center), |ui| {
                        ui.allocate_ui_with_layout([ui.min_size()[0], 0.].into(), egui::Layout::top_down(egui::Align::Center), |ui| {
                            if self.hashing_complete && self.filelist_loaded {
                                if ui.button("CLOSE").clicked() {
                                    self.show_hashing = false;
                                }
                            } else {
                                if ui.button("CANCEL").clicked() {
                                    self.show_hashing = false;
                                    self.hashing_cancelled.cancel();
                                }
                            }
                        });
                    });
            });
        };
    }
}

fn aspect_fit(img_size: impl Into<Vec2>, fit_size: impl Into<Vec2>) -> Vec2 {
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
    Vec2 {x: ratio*img_size.x, y: ratio*img_size.y}
}


impl eframe::App for IndexingGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::Area::new("mainarea").enabled(!(self.show_library_man||self.show_dialog_err||self.show_hashing)).show(ctx, |ui| self.deduplication_win(ui));
        });
        self.watch_dir_manager_win(ctx);
        self.hashing_progress_win(ctx);
        let scr_size = ctx.screen_rect().max;
        let scr_size = egui::Pos2 { x: (scr_size.x - 200.)/2., y: (scr_size.y - 200.)/2.};
        egui::Window::new("dialog_error").default_pos(scr_size).open(&mut self.show_dialog_err).fixed_size([200.,200.]).show(ctx, |ui| {
            ui.colored_label(egui::Color32::RED, "ERROR: no system dialog found");
        });

    }

    fn on_exit(&mut self, _ctx: Option<&eframe::glow::Context>) {
        self.cancel_token.cancel();
        Arc::try_unwrap(self.rt.take().unwrap()).unwrap().shutdown_background();
    }
}