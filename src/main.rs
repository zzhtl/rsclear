fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("rsclear · 磁盘空间管理")
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rsclear",
        options,
        Box::new(|cc| Ok(Box::new(rsclear::ui::App::new(cc)?))),
    )
}
