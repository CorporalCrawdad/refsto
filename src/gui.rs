use eframe::egui;
use egui_extras::RetainedImage;
use tokio::runtime;
use std::sync::mpsc::{channel, Receiver, Sender};

#[derive(Debug,PartialEq)]
enum ScaleMode {
    Scaled,
    Fit,
    Expand,
}

pub struct Glowie {
    filelist: Vec<std::path::PathBuf>,
    fileindex: usize,
    img: Option<RetainedImage>,
    scale: f32,
    scalemode: ScaleMode,
    scr_w: usize,
    scr_h: usize,
    rt: runtime::Runtime,
    image_rx: Receiver<Option<RetainedImage>>,
    image_tx: Sender<Option<RetainedImage>>,
    loading: bool,
}

impl Glowie {
    pub fn new(_cc: &eframe::CreationContext<'_>, path: impl AsRef<std::path::Path>) -> Self {
        // Customize egui here with cc.egui_ctx.set_fonts and cc.egui_ctx.set_visuals.
        // Restore app state using cc.storage (requires the "persistence" feature).
        // Use the cc.gl (a glow::Context) to create graphics shaders and buffers that you can use
        // for e.g. egui::PaintCallback.

        // Get list of files from provided directory
        let path = path.as_ref();
        let mut filelist: _ = vec![]; 
        if path.is_dir() {
            match std::fs::read_dir(path) {
                Ok(readdir) => {
                    filelist = readdir.filter(|entry| entry.is_ok() && entry.as_ref().unwrap().file_type().unwrap().is_file()).flatten().map(|entry| entry.path()).collect();
                },
                Err(_) => panic!("Could not open provided directory {}", path.display()),
            };
        };
        
        // Load first image
        // let mut ri: Option<RetainedImage> = None;
        // while filelist.len() > 0 && ri.is_none() {
        //     let img = RetainedImage::from_image_bytes(filelist[0].to_string_lossy(), &std::fs::read(&filelist[0]).unwrap());
        //     if img.is_ok() {
        //         ri = Some(img.unwrap());
        //         for entry in &filelist {
        //             println!("{}", entry.display());
        //         }
        //         println!("Loading entry {}, image {}!", 0, ri.as_ref().unwrap().debug_name());
        //     } else {
        //         filelist.remove(0);
        //     }
        // }
        let (sender, receiver) = channel();

        Self{
            filelist,
            fileindex: 0,
            img: None,
            scale: 5f32,
            scalemode: ScaleMode::Expand,
            scr_w: 0, scr_h: 0,
            rt: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
            image_rx: receiver,
            image_tx: sender,
            loading: false,
        }
    }

    async fn load_pic(path: impl AsRef<std::path::Path>, tx: Sender<Option<RetainedImage>>) {
        let path: &std::path::Path = path.as_ref();
        // println!("Attempting to load {} into memory...", path.to_string_lossy());
        if let Ok(bytes) = &std::fs::read(path) {
            if let Ok(img) = RetainedImage::from_image_bytes(path.to_string_lossy(), bytes) {
                tx.send(Some(img)).expect("Channel send error");
            } else {
                tx.send(None).expect("Channel send error");
            }
        } else {
            tx.send(None).expect("Channel send error");
        }
    }

    fn spawn_next_pic(&mut self) {
        if let Some(path) = self.filelist.get(self.fileindex) {
            let tx = self.image_tx.clone();
            let path = path.clone();
            self.rt.spawn(async move {
                Self::load_pic(path, tx).await;
            });
            self.loading = true;
        }
    }
}

impl eframe::App for Glowie {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // let rt = egui::RichText::new("A label").color(egui::Color32::from_rgb(0, 255, 255)).strikethrough();
            // ui.label(rt);
            // ui.heading("Hello Wrlod!");
            egui::ComboBox::from_label("Image Scaling").selected_text(format!("{:?}", self.scalemode)).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.scalemode, ScaleMode::Scaled, "Scaled");
                ui.selectable_value(&mut self.scalemode, ScaleMode::Fit, "Fit");
                ui.selectable_value(&mut self.scalemode, ScaleMode::Expand, "Expand");
            });
            if self.scalemode == ScaleMode::Scaled {
                ui.add(
                    egui::Slider::new(&mut self.scale, 1.0..=100.0).clamp_to_range(false).text("Image scale: ")
                );
            }
            if self.filelist.len() > 0 {
                if ui.button("Next Image").clicked() {
                    self.fileindex += 1;
                    if self.fileindex == self.filelist.len() {
                        self.fileindex = 0;
                    }
                    self.img = None;
                }
            }
            if self.filelist.len() > 0 {
                // println!("Filelist not empty!");
                if let Some(img) = &self.img {
                    let [w, h] = img.size();
                    let [mut w, mut h] = [w as f32,h as f32];
                    let (scr_w, scr_h) = ui.available_size().into();
                    if scr_w as usize != self.scr_w || scr_h as usize != self.scr_h {
                        println!("size change: {}x{}\nimg size: {}x{}", scr_w, scr_h, w, h);
                        [self.scr_w, self.scr_h] = [scr_w as usize, scr_h as usize];
                    }
                    match self.scalemode {
                        ScaleMode::Expand => {
                            let w_rat = scr_w/w;
                            let h_rat = scr_h/h;
                            if w_rat < h_rat {
                                [w,h] = [w*w_rat, h*w_rat];
                            } else {
                                [w,h] = [w*h_rat, h*h_rat];
                            }
                        },
                        ScaleMode::Fit=> {
                            let w_rat = scr_w/w;
                            let h_rat = scr_h/h;
                            if w_rat<1f32 || h_rat<1f32 {
                                if w_rat < h_rat {
                                    [w,h] = [w*w_rat, h*w_rat];
                                } else {
                                    [w,h] = [w*h_rat, h*h_rat];
                                }
                            }
                        },
                        ScaleMode::Scaled=> {
                            [w,h] = [self.scale*w/100f32, self.scale*h/100f32];
                        },
                    }
                    ui.add(
                        egui::Image::new(img.texture_id(ctx), (w,h))
                    );
                } else if self.loading {
                    ctx.request_repaint();
                    if let Ok(rx) = self.image_rx.try_recv() {
                        if rx.is_none() {
                            if self.fileindex < self.filelist.len() {
                                self.filelist.remove(self.fileindex);
                                self.spawn_next_pic();
                            }
                        } else {
                            self.img = rx;
                            self.loading = false;
                        }
                    }
                } else { // !self.loading && self.img.is_none()
                    self.loading = true;
                    self.spawn_next_pic();
                }
            } else {
                ui.label("No valid images to show!");
            }
        });
    }
}
