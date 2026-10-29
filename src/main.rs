//! PDF Tool — сборка PDF из изображений, сжатие, удаление, добавление и перестановка страниц.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
mod i18n;

mod app;
mod compress;
mod edit;
mod editor;
mod images;
mod pdf_ops;
mod render;

use std::path::PathBuf;

fn main() -> eframe::Result {
    i18n::init();
    // Файл можно передать аргументом: pdf-tool документ.pdf
    let open = std::env::args_os().nth(1).map(PathBuf::from);

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("PDF Tool")
            .with_app_id("pdf-tool")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([900.0, 600.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "PDF Tool",
        options,
        Box::new(|cc| Ok(Box::new(app::PdfToolApp::new(cc, open)))),
    )
}
