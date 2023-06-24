mod gui;
mod index;
use crate::gui::Glowie;

fn main() {
    let native_options = eframe::NativeOptions::default();
    let _ = eframe::run_native("Glowie", native_options, Box::new(|cc| Box::new(Glowie::new(cc, "./"))));
}