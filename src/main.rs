mod binpare_app;
use binpare_app::BinpareApp;

fn main() {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "binpare",
        options,
        Box::new(|cc| Ok(Box::new(BinpareApp::new(cc)))),
    )
    .expect("failed to start eframe");
}
