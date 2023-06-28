use eframe::egui;
use tokio::runtime;
use std::{sync::{{Arc, Mutex}}, path::{Path,PathBuf}};

pub struct IndexingGui {
    filelist: Arc<Mutex<Option<Vec<PathBuf>>>>,
    rt: runtime::Runtime,
}

impl IndexingGui {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl Into<PathBuf>) -> Self {
        let ig = IndexingGui { filelist: Arc::new(Mutex::new(None)), rt: tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap() };
        let fl_handle = ig.filelist.clone();
        let path = path.into();
        ig.rt.spawn(async move {
            Self::load_filelist(path, fl_handle).await;
        });
        ig
    }

    async fn load_filelist(path: impl AsRef<Path> + std::marker::Send, filelist: Arc<Mutex<Option<Vec<PathBuf>>>>) {
        let path = path.as_ref();
        if path.is_dir() {
            match std::fs::read_dir(path) {
                Ok(readdir) => {
                    *filelist.lock().unwrap() = Some(readdir.filter(|entry| entry.is_ok() && entry.as_ref().unwrap().file_type().unwrap().is_file()).flatten().map(|entry| entry.path()).collect());
                },
                Err(_) => panic!("Could not open directory {}", path.display()),
            }
        }
    }
}

impl eframe::App for IndexingGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            let lock = self.filelist.try_lock();
            match lock {
                Ok(mutguard) => {
                    if let Some(filelist) = &*mutguard {
                        for path in filelist {
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