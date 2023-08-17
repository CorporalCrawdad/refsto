// use eframe::{egui::{self, RichText}, epaint::Color32};
use eframe::{egui, egui::{RichText, Color32, Vec2, Rect, Ui}};
use egui_extras::RetainedImage;
use tokio::runtime;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use std::{sync::{{Arc, RwLock}, atomic::{Ordering::Relaxed, AtomicI64}, mpsc, mpsc::TryRecvError}, path::PathBuf, collections::HashSet};
use futures::stream::futures_unordered::FuturesUnordered;
use sqlx::{Row,Acquire};

use crate::index::{HashIndexer, HashIndexError};

const CHECKMARK: &[u8] = include_bytes!("../assets/checkmark.png");

#[derive(PartialEq)]
enum PopOvers {
    LibraryManager,
    HashingDbUpdate,
    BinaryDedup(BinDedupStep),
    None
}

#[derive(Copy, Clone, PartialEq)]
pub enum KeepWhichFile {
    CreatedFirst, // ctime
    ModifiedFirst, // mtime
    PathShortest, // char count of full path
    NameShortest, // char count of filename
    PathShallowest, // least directories deep
}

#[derive(PartialEq)]
enum BinDedupStep {
    SelectMethod,
    Loading,
    ReviewFilelist,
    _Deletion,
}

pub enum BinDupeMessage {
    NewSet,
    Entry(PathBuf),
}

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
    hamming_proximity: usize,
    filelist: HashSet<PathBuf>,
    filelist_loaded: bool,
    rehashed_cnt: usize,
    rehashed_cnt_recv: Option<mpsc::Receiver<usize>>,
    popover: PopOvers,
    error_no_dialogs: bool,
    watched_image_count: Arc<AtomicI64>,
    bin_dedup_method: KeepWhichFile,
    bin_dedup_method_reversed: bool,
    bin_dupes: Vec<HashSet<PathBuf>>,
    bin_dupes_recv: Option<mpsc::Receiver<BinDupeMessage>>,
    incl_ignored: bool,
    // bin_dedup_step: BinDedupStep,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, rt: Arc<runtime::Runtime>, db_pool: sqlx::SqlitePool) -> Self {
        let (set_images_tx, set_images_rx) = std::sync::mpsc::channel();
        let mut ig = IndexingGui {
            watched_dirs: Arc::new(RwLock::new(HashSet::new())),
            watched_image_count: Arc::new(AtomicI64::new(0)),
            rt: Some(rt),
            cancel_token: CancellationToken::new(),
            hashing_cancelled: CancellationToken::new(),
            checkmark: RetainedImage::from_image_bytes("checkmark", CHECKMARK).unwrap(),
            set_images: vec![],
            set_images_recv: set_images_rx,
            db_pool: db_pool.clone(),
            hamming_proximity: 0,
            filelist: HashSet::new(),
            filelist_recv: None,
            filelist_loaded: false,
            hashing_complete: true,
            rehashed_cnt: 0,
            rehashed_cnt_recv: None,
            popover: PopOvers::None,
            error_no_dialogs: false,
            bin_dedup_method: KeepWhichFile::CreatedFirst,
            bin_dedup_method_reversed: false,
            bin_dupes: vec![],
            bin_dupes_recv: None,
            incl_ignored: false,
            // bin_dedup_step: BinDedupStep::SelectMethod,
        };

        // dummy code that loads images from disk into ui
        let wic = ig.watched_image_count.clone();
        let conn = ig.db_pool.clone();
        ig.rt.as_ref().unwrap().spawn(async move {
            wic.store(sqlx::query("SELECT COUNT(*) FROM entries WHERE ignored = 0;").fetch_one(conn.acquire().await.unwrap().acquire().await.unwrap()).await.unwrap().get::<i64,_>(0), Relaxed);
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

    fn spawn_load_filelist(&mut self) {
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
                fut_set.push(async move {
                    let mut dirlist = vec![dir];
                    while let Some(dir) = dirlist.pop() {
                        if let Ok(entries) = dir.read_dir() {
                            let entries: Vec<std::fs::DirEntry> = entries.into_iter().flatten().collect();
                            for entry in entries {
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
                })
            }
            let ct = self.hashing_cancelled.clone();
            self.rt.as_ref().unwrap().spawn(async move {
                while let Some(_) = fut_set.next().await{ if ct.is_cancelled() { break } }
            });
        }
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

    // fn spawn_find_dupes(rt_handle: &runtime::Handle, cancel_token: CancellationToken, db_pool: sqlx::SqlitePool, hash_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, bin_dupes: Arc<tokio::sync::RwLock<Vec<Vec<String>>>>, hamming_proximity: usize, done_flag: Arc<AtomicBool>) {
    //     rt_handle.spawn(async move {
    //         let hi = HashIndexer::new(db_pool);
    //         hi.cluster(hash_dupes, bin_dupes, hamming_proximity, cancel_token).await;
    //         done_flag.store(true, Relaxed);
    //     });
    // }

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
                    if ui.button("Deduplicate Exact Matches").clicked() { self.popover = PopOvers::BinaryDedup(BinDedupStep::SelectMethod) }
                    ui.separator();
                    ui.add(egui::widgets::DragValue::new(&mut self.hamming_proximity).clamp_range(0..=100));
                    ui.label(RichText::new("% different by hash").color(Color32::BLACK));
                    if ui.button("LOAD").clicked() {
                    }
                    // change to RTL
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.button("Library settings")
                            .on_hover_text_at_pointer(RichText::new("After managing watched directories, click\nRELOAD to refetch list of files and \nupdate/re-hash them into the database.").color(egui::Color32::WHITE))
                            .clicked() {
                            self.popover = PopOvers::LibraryManager;
                            self.filelist_loaded = false;
                        }
                        ui.separator();
                        ui.label(RichText::new(format!("{} managed images", self.watched_image_count.load(Relaxed))).color(Color32::BLACK));
                        if ui.button("RELOAD").clicked() {
                            self.popover = PopOvers::HashingDbUpdate;
                            self.filelist_loaded = false;
                            self.hashing_cancelled = CancellationToken::new();
                        }
                        if ui.button("CLEAN MISSING").clicked() {
                            let conn = self.db_pool.clone();
                            let cloned_list = self.filelist.clone();
                            self.rt.as_ref().unwrap().spawn(async move {
                                let stored_filelist: HashSet<PathBuf> = sqlx::query("SELECT * FROM entries").fetch_all(conn.acquire().await.unwrap().acquire().await.unwrap()).await.unwrap().iter().map(|x| PathBuf::from(x.get::<String,_>("filepath"))).collect();
                                let deleted_files: Vec<String> = stored_filelist.difference(&cloned_list).map(|x| x.to_string_lossy().to_string()).collect();
                                println!("Files missing from filesystem:\n{:?}", deleted_files);
                            });
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
        popover_frame("dirwatchwin", ctx, Some([400.,600.].into()), |ui| {
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
                            self.error_no_dialogs = true;
                        }
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.add(egui::Button::new(RichText::new("🗙").color(Color32::WHITE).strong().size(20.)).fill(Color32::LIGHT_RED)).clicked() {
                            self.popover = PopOvers::None;
                        }
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
        });
        if self.error_no_dialogs {
            popover_frame("Dialog Error", ctx, Some([200.,200.].into()), |ui| {
                ui.colored_label(egui::Color32::RED, "ERROR: no system dialog found");
                self.error_no_dialogs = !ui.button("OK").clicked();
            });
        }
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

    fn hashing_progress_win(&mut self, ctx: &egui::Context) {
        popover_frame("Hashing Progress Window", ctx, None, |ui| {
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

                                // let conn = self.db_pool.clone();
                                // let cloned_list = self.filelist.clone();
                                // self.rt.as_ref().unwrap().spawn(async move {
                                //     let stored_filelist: HashSet<PathBuf> = sqlx::query("SELECT * FROM entries").fetch_all(conn.acquire().await.unwrap().acquire().await.unwrap()).await.unwrap().iter().map(|x| PathBuf::from(x.get::<String,_>("filepath"))).collect();
                                //     let deleted_files: Vec<String> = stored_filelist.difference(&cloned_list).map(|x| x.to_string_lossy().to_string()).collect();
                                //     println!("Files missing from filesystem:\n{:?}", deleted_files);
                                // });

                                let wic = self.watched_image_count.clone();
                                let conn = self.db_pool.clone();
                                self.rt.as_ref().unwrap().spawn(async move {
                                    while let Some(_) = fut_set.next().await { if ct.is_cancelled() { break };}
                                    wic.store(sqlx::query("SELECT COUNT(*) FROM entries WHERE ignored = 0;").fetch_one(conn.acquire().await.unwrap().acquire().await.unwrap()).await.unwrap().get::<i64,_>(0), Relaxed);
                                });
                                break
                            }
                        }
                    }
                    while let Ok(filename) = filelist_recv.try_recv() {
                        self.filelist.insert(filename);
                    }
                },
                None => self.spawn_load_filelist()
                }
            } else {
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
            hcenter_no_expand(ui, |ui| {
                let text = if self.hashing_complete && self.filelist_loaded {
                        "CLOSE"
                    } else {
                        "CANCEL"
                    };
                if ui.button(text).clicked() {
                    self.popover = PopOvers::None;
                    self.hashing_cancelled.cancel();
                }
            });
        });
    }

    fn binary_dedup_win(&mut self, ctx: &egui::Context) {
        match self.popover {
            PopOvers::BinaryDedup(BinDedupStep::SelectMethod) => {
                popover_frame("Binary Deduplicator", ctx, Some([270.,270.].into()), |ui| {
                    ui.label(RichText::new("Delete exact duplicates of images").text_style(egui::TextStyle::Heading).color(Color32::BLACK));
                    hcenter_no_expand(ui, |ui| {ui.separator();});
                    ui.colored_label(Color32::BLACK, "Which duplicate should be kept:");
                    let width = ui.min_size().x;
                    egui::Frame::none()
                        .fill(Color32::from_rgb(200, 190, 164))
                        .inner_margin(egui::Margin::symmetric(5., 5.))
                        .outer_margin(egui::Margin::symmetric(5., 5.))
                        .show(ui, |ui| {
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::CreatedFirst,   RichText::new("Created first").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::ModifiedFirst,  RichText::new("Modified least recently").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::NameShortest,   RichText::new("Shortest name").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::PathShallowest, RichText::new("Shallowest path").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::PathShortest,   RichText::new("Shortest path").color(Color32::BLACK));
                            ui.allocate_space([width-20., 0.].into());
                            hcenter_no_expand(ui, |ui| {
                                ui.separator();
                                ui.checkbox(&mut self.bin_dedup_method_reversed, "Reverse order").on_hover_text_at_pointer("Switches order, i.e.: Created First → Created Last");
                            });
                        });
                    egui::Frame::none()
                        .fill(Color32::from_rgb(200, 190, 164))
                        .inner_margin(egui::Margin::symmetric(5., 5.))
                        .outer_margin(egui::Margin::symmetric(5., 5.))
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout([width-20.,0.].into(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Checkbox::without_text(&mut self.incl_ignored));
                                if ui.add(egui::Label::new(RichText::new("Dedupe ignored files").color(Color32::BLACK)).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::Help).on_hover_text_at_pointer(RichText::new("Also delete duplicated non-image files hashed in database")).clicked() {
                                    self.incl_ignored = !self.incl_ignored;
                                }
                            });
                            ui.allocate_space([width-20., 0.].into());
                        });
                    hcenter_no_expand(ui, |ui| {
                        if ui.button("OK?").clicked() {
                            let hi = HashIndexer::new(self.db_pool.clone());
                            let incl_ignored = self.incl_ignored;
                            let (tx, rx) = mpsc::channel();
                            let method = self.bin_dedup_method;
                            self.bin_dupes_recv = Some(rx);
                            self.rt.as_ref().unwrap().spawn(async move {
                                hi.find_bindupes(incl_ignored, method, tx).await;
                            });
                            self.popover = PopOvers::BinaryDedup(BinDedupStep::Loading);
                        }
                    });
                });
            },
            PopOvers::BinaryDedup(BinDedupStep::Loading) => {
                let mut drop_recv = false;
                match &self.bin_dupes_recv {
                    Some(rx) => {
                        match rx.try_recv() {
                            Ok(msg) => match msg {
                                    BinDupeMessage::NewSet => self.bin_dupes.push(HashSet::new()),
                                    BinDupeMessage::Entry(path) => { self.bin_dupes.last_mut().expect("Tried inserting to bin_dupes before creating HashSet").insert(path); },
                                },
                            Err(TryRecvError::Empty) => (),
                            Err(TryRecvError::Disconnected) => {
                                drop_recv = true;
                            }
                        }
                    },
                    None => {
                        self.popover = PopOvers::BinaryDedup(BinDedupStep::ReviewFilelist);
                    },
                }
                if drop_recv { self.bin_dupes_recv = None };
                popover_frame("Binary Deduplicator", ctx, Some([270.,270.].into()), |ui| {
                    ui.add_enabled_ui(false, |ui| {
                        ui.label(RichText::new("Delete exact duplicates of images").text_style(egui::TextStyle::Heading).color(Color32::BLACK));
                        hcenter_no_expand(ui, |ui| {ui.separator();});
                        ui.colored_label(Color32::BLACK, "Which duplicate should be kept:");
                        let width = ui.min_size().x;
                        egui::Frame::none()
                        .fill(Color32::from_rgb(200, 190, 164))
                        .inner_margin(egui::Margin::symmetric(5., 5.))
                        .outer_margin(egui::Margin::symmetric(5., 5.))
                        .show(ui, |ui| {
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::CreatedFirst,   RichText::new("Created first").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::ModifiedFirst,  RichText::new("Modified least recently").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::NameShortest,   RichText::new("Shortest name").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::PathShallowest, RichText::new("Shallowest path").color(Color32::BLACK));
                            ui.radio_value(&mut self.bin_dedup_method, KeepWhichFile::PathShortest,   RichText::new("Shortest path").color(Color32::BLACK));
                            ui.allocate_space([width-20., 0.].into());
                            hcenter_no_expand(ui, |ui| {
                                ui.separator();
                                ui.checkbox(&mut self.bin_dedup_method_reversed, "Reverse order").on_hover_text_at_pointer("Switches order, i.e.: Created First → Created Last");
                            });
                        });
                    egui::Frame::none()
                        .fill(Color32::from_rgb(200, 190, 164))
                        .inner_margin(egui::Margin::symmetric(5., 5.))
                        .outer_margin(egui::Margin::symmetric(5., 5.))
                        .show(ui, |ui| {
                            ui.allocate_ui_with_layout([width-20.,0.].into(), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                ui.add(egui::Checkbox::without_text(&mut self.incl_ignored));
                                if ui.add(egui::Label::new(RichText::new("Dedupe ignored files").color(Color32::BLACK)).sense(egui::Sense::click())).on_hover_cursor(egui::CursorIcon::Help).on_hover_text_at_pointer(RichText::new("Also delete duplicated non-image files hashed in database")).clicked() {
                                    self.incl_ignored = !self.incl_ignored;
                                }
                            });
                        ui.allocate_space([width-20., 0.].into());
                        });
                    });
                    hcenter_no_expand(ui, |ui| {
                        ui.label(RichText::new({
                            match ui.input(|i| i.time as usize) % 4 {
                                0 => "(Loading    )",
                                1 => "(Loading .  )",
                                2 => "(Loading .. )",
                                3 => "(Loading ...)",
                                _ => "what time is it?!",
                            }
                        }).monospace().color(Color32::BLACK));
                        ctx.request_repaint();
                    });
                });
            },
            PopOvers::BinaryDedup(BinDedupStep::ReviewFilelist) => {
                popover_frame("Binary Deduplicator", ctx, Some([270.,270.].into()), |ui| {
                    ui.label(RichText::new("Delete exact duplicates of images").text_style(egui::TextStyle::Heading).color(Color32::BLACK));
                    hcenter_no_expand(ui, |ui| {ui.separator();});
                    egui::ScrollArea::both().max_height(270.).max_width(270.).show(ui, |ui| {
                        for bindup_set in &self.bin_dupes {
                            ui.horizontal(|ui| {
                                for entry in bindup_set {
                                    ui.label(entry.to_string_lossy().to_string());
                                }
                            });
                            ui.allocate_space([0.,5.].into());
                        }
                    });
                });
            }
            _ => panic!(),
        }
    }
}

fn hcenter_no_expand<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
            ui.allocate_ui_with_layout([ui.min_size()[0], 0.].into(), egui::Layout::top_down(egui::Align::Center), add_contents)
}

fn popover_frame<R>(id: impl Into<egui::Id>, ctx: &egui::Context, size: Option<Vec2>, add_contents: impl FnOnce(&mut Ui) -> R) -> egui::InnerResponse<R> {
    egui::Area::new(id).movable(false).order(egui::Order::Foreground).anchor(egui::Align2::CENTER_CENTER, [0.,0.]).show(ctx, |ui| {
        egui::Frame::none()
            .fill(egui::Color32::GRAY)
            .stroke(egui::Stroke::new(3., egui::Color32::BLACK))
            .rounding(egui::Rounding::same(4.))
            .inner_margin(egui::Margin::symmetric(3., 3.))
            .show(ui, |ui| {
                if let Some(size) = size {
                    ui.set_max_size(size);
                    let ret = add_contents(ui);
                    ui.allocate_space(ui.available_size());
                    ret
                } else {
                    add_contents(ui)
                }
            }).inner
    })
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
        egui::Area::new("mainarea")
            .enabled(self.popover == PopOvers::None)
            .show(ctx, |ui| self.deduplication_win(ui));

        match self.popover {
            PopOvers::BinaryDedup(_) => self.binary_dedup_win(ctx),
            PopOvers::HashingDbUpdate => self.hashing_progress_win(ctx),
            PopOvers::LibraryManager => self.watch_dir_manager_win(ctx),
            PopOvers::None => ()
        }
    }

    fn on_exit(&mut self, _ctx: Option<&eframe::glow::Context>) {
        self.cancel_token.cancel();
        Arc::try_unwrap(self.rt.take().unwrap()).unwrap().shutdown_background();
    }
}