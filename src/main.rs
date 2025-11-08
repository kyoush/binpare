use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::io::Read;

const DEFAULT_BYTES_PER_ROW: usize = 16;
const MAX_READ_BYTES: u64 = 100 * 1024 * 1024; // 100 MB guard for initial implementation

fn main() {
    let native_options = eframe::NativeOptions::default();
    let _ = eframe::run_native(
        "binpare",
        native_options,
        Box::new(|cc| Ok(Box::new(BinpareApp::new(cc)))),
    );
}

#[allow(dead_code)]
struct LoadedFile {
    path: PathBuf,
    data: Arc<Mutex<Vec<u8>>>,
    size: u64,
    truncated: bool,
    ready: Arc<AtomicBool>,
}

struct BinpareApp {
    file_a: Option<LoadedFile>,
    file_b: Option<LoadedFile>,
    split_mode: bool,
    bytes_per_row: usize,
    loading: bool,
    dialog_rx: Option<Receiver<(usize, Option<PathBuf>)>>,
    // cached diff vector for current file_a vs file_b (computed once when both ready)
    diff: Option<Vec<bool>>,
}

impl BinpareApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Try to load a Japanese-capable font from common system paths.
        let mut fonts = egui::FontDefinitions::default();

        let japanese_font_paths = [
            "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "C:\\Windows\\Fonts\\msgothic.ttc",
            "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        ];

        for p in japanese_font_paths.iter() {
            if let Ok(bytes) = std::fs::read(p) {
                fonts.font_data.insert(
                    "japanese".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "japanese".to_owned());
                break;
            }
        }

        cc.egui_ctx.set_fonts(fonts);

        Self {
            file_a: None,
            file_b: None,
            split_mode: false,
            bytes_per_row: DEFAULT_BYTES_PER_ROW,
            loading: false,
            dialog_rx: None,
            diff: None,
        }
    }

    #[allow(dead_code)]
    fn open_file_in_background(&mut self, path: PathBuf, target: &mut Option<LoadedFile>) {
        // kept for compatibility but forward to spawn_loaded_file
        *target = Some(BinpareApp::spawn_loaded_file(path));
    }

    fn spawn_loaded_file(path: PathBuf) -> LoadedFile {
    let data_arc = Arc::new(Mutex::new(Vec::new()));
    let data_clone = data_arc.clone();
    let ready = Arc::new(AtomicBool::new(false));
    let ready_clone = ready.clone();
        let path_clone = path.clone();

        thread::spawn(move || {
            if let Ok(metadata) = std::fs::metadata(&path_clone) {
                let size = metadata.len();
                let to_read = std::cmp::min(size, MAX_READ_BYTES);
                if let Ok(file) = std::fs::File::open(&path_clone) {
                    let mut tmp = Vec::new();
                    let _ = file.take(to_read).read_to_end(&mut tmp);
                    let mut locked = data_clone.lock().unwrap();
                    *locked = tmp;
                    ready_clone.store(true, Ordering::Release);
                }
            }
        });

        LoadedFile {
            path,
            data: data_arc,
            size: 0,
            truncated: false,
            ready,
        }
    }
}

impl eframe::App for BinpareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // collect file-open requests from UI to avoid multiple simultaneous mutable borrows
        let mut to_open: Vec<(usize, PathBuf)> = Vec::new();
        // Menu bar
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("binpare");
                ui.push_id("open_a", |ui| {
                    if ui.button("Open A").clicked() {
                        // spawn native file dialog in background thread to avoid blocking UI thread
                        if self.dialog_rx.is_none() {
                            let (tx, rx) = channel();
                            self.dialog_rx = Some(rx);
                            thread::spawn(move || {
                                let res = rfd::FileDialog::new().pick_file();
                                let _ = tx.send((0, res));
                            });
                        }
                    }
                });

                if self.split_mode {
                    ui.push_id("open_b", |ui| {
                        if ui.button("Open B").clicked() {
                            // open for file B
                            if self.dialog_rx.is_none() {
                                let (tx, rx) = channel();
                                self.dialog_rx = Some(rx);
                                thread::spawn(move || {
                                    let res = rfd::FileDialog::new().pick_file();
                                    let _ = tx.send((1, res));
                                });
                            }
                        }
                    });
                }

                ui.push_id("toggle_split", |ui| {
                    if ui.button(if self.split_mode { "Single view" } else { "Split view" }).clicked() {
                        self.split_mode = !self.split_mode;
                    }
                });

                ui.push_id("bytes_per_row", |ui| {
                    ui.add(egui::Slider::new(&mut self.bytes_per_row, 8..=32).text("bytes/row"));
                });
            });
        });

        // Handle drag and drop (copy raw dropped files out of ctx safely)
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            for (i, f) in dropped.iter().enumerate() {
                if let Some(path) = &f.path {
                    to_open.push((i, path.clone()));
                }
            }
            // if two files were dropped, switch to split mode
            if dropped.len() >= 2 {
                self.split_mode = true;
            }
        }

        // poll dialog receiver if present (before consuming to_open)
        if let Some(rx) = &self.dialog_rx {
            match rx.try_recv() {
                Ok((target, maybe_path)) => {
                    if let Some(path) = maybe_path {
                        to_open.push((target, path));
                    }
                    self.dialog_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // still waiting for dialog result
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.dialog_rx = None;
                }
            }
        }

        // If both files are present and ready, compute diff once and cache it to avoid
        // recomputing every frame (major perf win in split view).
        if self.file_a.is_some() && self.file_b.is_some() {
            let a_ready = self.file_a.as_ref().unwrap().ready.load(Ordering::Acquire);
            let b_ready = self.file_b.as_ref().unwrap().ready.load(Ordering::Acquire);
            if a_ready && b_ready && self.diff.is_none() {
                // acquire locks, compute diff, then drop locks
                let a_data = self.file_a.as_ref().unwrap().data.lock().unwrap();
                let b_data = self.file_b.as_ref().unwrap().data.lock().unwrap();
                let max = std::cmp::max(a_data.len(), b_data.len());
                let mut diffv = vec![false; max];
                for i in 0..max {
                    if a_data.get(i).copied() != b_data.get(i).copied() {
                        diffv[i] = true;
                    }
                }
                self.diff = Some(diffv);
            }
        } else {
            // no pair to diff
            self.diff = None;
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                if self.file_a.is_none() {
                    ui.label("No file opened. Use Open button or drag & drop a file.");
                }
            });

            ui.separator();

            if self.split_mode {
                ui.columns(2, |cols| {
                    cols[0].vertical(|ui| {
                        if let Some(loaded) = &self.file_a {
                            let ready = loaded.ready.load(Ordering::Acquire);
                            let data = loaded.data.lock().unwrap();
                            if !ready {
                                ui.centered_and_justified(|ui| { ui.label("Loading file A..."); });
                            } else {
                                // use cached diff if available
                                ui.push_id("pane_a", |ui| {
                                    hexdump_ui(ui, &data, self.bytes_per_row, self.diff.as_ref());
                                });
                            }
                        } else {
                            ui.label("(no file)");
                        }
                    });

                    cols[1].vertical(|ui| {
                        if let Some(loaded) = &self.file_b {
                            let ready = loaded.ready.load(Ordering::Acquire);
                            let data = loaded.data.lock().unwrap();
                            if !ready {
                                ui.centered_and_justified(|ui| { ui.label("Loading file B..."); });
                            } else {
                                // use cached diff if available
                                ui.push_id("pane_b", |ui| {
                                    hexdump_ui(ui, &data, self.bytes_per_row, self.diff.as_ref());
                                });
                            }
                        } else {
                            ui.label("(no file)");
                        }
                    });
                });
            } else {
                render_pane_single(ui, &self.file_a, self.bytes_per_row);
            }
        });

        // perform deferred open operations
        for (i, path) in to_open {
            if i == 0 {
                self.file_a = Some(BinpareApp::spawn_loaded_file(path));
                self.loading = true;
                self.diff = None;
            } else {
                // second dropped/opened file -> assign to file_b and enable split
                self.file_b = Some(BinpareApp::spawn_loaded_file(path));
                self.loading = true;
                self.split_mode = true;
                self.diff = None;
            }
        }
    }
}

fn render_pane_single(ui: &mut egui::Ui, file: &Option<LoadedFile>, bytes_per_row: usize) {
    ui.group(|ui| {
        if let Some(loaded) = file {
            let data = loaded.data.lock().unwrap();
            ui.push_id("single_pane", |ui| {
                hexdump_ui(ui, &data, bytes_per_row, None);
            });
        } else {
            ui.label("(no file)");
        }
    });
}

#[allow(dead_code)]
fn render_pane(mut col: egui::Ui, file: &Option<LoadedFile>, other: &Option<LoadedFile>, bytes_per_row: usize) {
    col.group(|ui| {
        if let Some(loaded) = file {
            let data = loaded.data.lock().unwrap();
            // create diff vector if other exists
            let diff = if let Some(o) = other {
                let d = o.data.lock().unwrap();
                let max = std::cmp::max(data.len(), d.len());
                let mut diffvec = vec![false; max];
                for i in 0..max {
                    let a = data.get(i).copied();
                    let b = d.get(i).copied();
                    if a != b {
                        diffvec[i] = true;
                    }
                }
                Some(diffvec)
            } else {
                None
            };
            hexdump_ui(ui, &data, bytes_per_row, diff.as_ref());
        } else {
            ui.label("(no file)");
        }
    });
}

fn hexdump_ui(ui: &mut egui::Ui, data: &[u8], bytes_per_row: usize, diff_opt: Option<&Vec<bool>>) {
    use egui::{Color32, RichText};
    let rows = (data.len() + bytes_per_row - 1) / bytes_per_row;
    let row_height = 18.0;
    egui::ScrollArea::vertical().show_rows(ui, row_height, rows, |ui, row_range| {
            for row in row_range {
                let offset = row * bytes_per_row;
                ui.push_id(row, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{:08x}:", offset)).monospace());

                        // Build hex and ascii strings for this row
                        let mut hex_line = String::with_capacity(bytes_per_row * 3);
                        let mut ascii = String::with_capacity(bytes_per_row);
                        let mut row_has_diff = false;
                        const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
                        for i in 0..bytes_per_row {
                            let idx = offset + i;
                            if idx < data.len() {
                                let b = data[idx];
                                // append two hex chars
                                hex_line.push(HEX_CHARS[(b >> 4) as usize] as char);
                                hex_line.push(HEX_CHARS[(b & 0xF) as usize] as char);
                                hex_line.push(' ');

                                // ascii
                                if b.is_ascii_graphic() || b == b' ' {
                                    ascii.push(b as char);
                                } else {
                                    ascii.push('.');
                                }

                                if let Some(diffv) = diff_opt {
                                    if idx < diffv.len() && diffv[idx] {
                                        row_has_diff = true;
                                    }
                                }
                            } else {
                                hex_line.push_str("   ");
                                ascii.push(' ');
                            }
                        }

                        // Color entire row if any byte differs (faster than per-byte coloring)
                        let mut rt_hex = RichText::new(hex_line).monospace();
                        let mut rt_ascii = RichText::new(ascii).monospace();
                        if row_has_diff {
                            let c = Color32::from_rgb(220, 100, 100);
                            rt_hex = rt_hex.color(c);
                            rt_ascii = rt_ascii.color(c);
                        }

                        ui.add(egui::Label::new(rt_hex));
                        ui.separator();
                        ui.add(egui::Label::new(rt_ascii));
                    });
                });
        }
    });
}
