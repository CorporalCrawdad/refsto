use eframe::egui;
use tokio::runtime;
use std::{sync::{{Arc, RwLock}}, path::PathBuf};

use crate::index::HashIndexer;

pub struct IndexingGui {
    filelist: Arc<RwLock<Option<Vec<PathBuf>>>>,
    rt: Box<Arc<runtime::Runtime>>,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl Into<PathBuf>, rt: Box<Arc<runtime::Runtime>>, db_pool: sqlx::SqlitePool) -> Self {
        let ig = IndexingGui { filelist: Arc::new(RwLock::new(None)), rt};
        let fl_handle = ig.filelist.clone();
        let path = path.into();
        ig.rt.spawn(async move {
            Self::load_filelist(path, fl_handle.clone()).await;
            Self::update_filelist(fl_handle.clone(), db_pool.clone()).await.unwrap();
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

    async fn update_filelist(filelist: Arc<RwLock<Option<Vec<PathBuf>>>>, db_pool: sqlx::SqlitePool) -> Result<(), anyhow::Error> {
        let hi = HashIndexer::new(db_pool);
        let lock = filelist.read();
        let mut fut_set = vec!();
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
                        ui.add(egui::Label::new(format!("Found {} files...", filelist.len())));
                        for path in filelist.into_iter().take(10) {
                            ui.add(egui::Label::new(path.as_path().to_string_lossy()));
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
}