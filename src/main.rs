mod gui;
mod index;
use crate::gui::IndexingGui;
use std::{env, path::Path};

fn main() {
    let args: Vec<String> = env::args().collect();
    let dir = args.get(1).unwrap_or(&String::from("./")).to_owned();
    let _ = eframe::run_native("Computing directory hashes...", eframe::NativeOptions::default(), Box::new(|cc| Box::new(IndexingGui::new(cc, dir))));
    // let _ = eframe::run_native("Glowie", eframe::NativeOptions::default(), Box::new(|cc| Box::new(Glowie::new(cc, dir))));
}