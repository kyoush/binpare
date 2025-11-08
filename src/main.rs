use eframe::egui;
use egui::{Color32, RichText};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::sync::atomic::{AtomicBool, Ordering};
use std::io::Read;

const MAX_READ_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB for demo
const DEFAULT_BYTES_PER_ROW: usize = 16;

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

    // diff cache between A and B (byte-level)
    diff: Option<Vec<bool>>,
    diff_highlight: bool,
    diff_row_indices: Vec<usize>,
    current_diff_idx: Option<usize>,
}

impl BinpareApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self {
            file_a: None,
            file_b: None,
            split_mode: false,
            bytes_per_row: DEFAULT_BYTES_PER_ROW,
            loading: false,
            dialog_rx: None,
            diff: None,
            diff_highlight: false,
            diff_row_indices: Vec::new(),
            current_diff_idx: None,
        }
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
                    let file = file;
                    let mut tmp = Vec::with_capacity(to_read as usize);
                    let _ = file.take(to_read).read_to_end(&mut tmp);
                    if let Ok(mut locked) = data_clone.lock() {
                        *locked = tmp;
                    }
                    ready_clone.store(true, Ordering::Release);
                }
            }
        });

        let (size, truncated) = match std::fs::metadata(&path) {
            Ok(m) => {
                let s = m.len();
                if s > MAX_READ_BYTES { (s, true) } else { (s, false) }
            }
            Err(_) => (0, false),
        };

        LoadedFile { path, data: data_arc, size, truncated, ready }
    }
}

impl eframe::App for BinpareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut to_open: Vec<(usize, PathBuf)> = Vec::new();

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("binpare");
                ui.separator();
                ui.checkbox(&mut self.diff_highlight, "Diff highlight");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Prev diff").clicked() {
                        if !self.diff_row_indices.is_empty() {
                            let next = match self.current_diff_idx {
                                Some(idx) => if idx == 0 { self.diff_row_indices.len() - 1 } else { idx - 1 },
                                None => 0,
                            };
                            self.current_diff_idx = Some(next);
                        }
                    }
                    if ui.button("Next diff").clicked() {
                        if !self.diff_row_indices.is_empty() {
                            let next = match self.current_diff_idx {
                                Some(idx) => (idx + 1) % self.diff_row_indices.len(),
                                None => 0,
                            };
                            self.current_diff_idx = Some(next);
                        }
                    }
                });

                ui.separator();

                if ui.button("Open A").clicked() {
                    if self.dialog_rx.is_none() {
                        let (tx, rx) = channel();
                        self.dialog_rx = Some(rx);
                        thread::spawn(move || {
                            let res = rfd::FileDialog::new().pick_file();
                            let _ = tx.send((0, res));
                        });
                    }
                }

                if self.split_mode {
                    if ui.button("Open B").clicked() {
                        if self.dialog_rx.is_none() {
                            let (tx, rx) = channel();
                            self.dialog_rx = Some(rx);
                            thread::spawn(move || {
                                let res = rfd::FileDialog::new().pick_file();
                                let _ = tx.send((1, res));
                            });
                        }
                    }
                }

                if ui.button(if self.split_mode { "Single view" } else { "Split view" }).clicked() {
                    self.split_mode = !self.split_mode;
                }
            });
        });

        // handle dropped files
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            for (i, f) in dropped.iter().enumerate() {
                if let Some(path) = &f.path {
                    to_open.push((i, path.clone()));
                }
            }
            if dropped.len() >= 2 {
                self.split_mode = true;
            }
        }

        // poll dialog receiver
        if let Some(rx) = &self.dialog_rx {
            match rx.try_recv() {
                Ok((target, maybe_path)) => {
                    if let Some(path) = maybe_path {
                        to_open.push((target, path));
                    }
                    self.dialog_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => { self.dialog_rx = None; }
            }
        }

        // perform deferred open operations
        for (i, path) in to_open {
            if i == 0 {
                self.file_a = Some(BinpareApp::spawn_loaded_file(path));
                self.loading = true;
                self.diff = None;
            } else {
                self.file_b = Some(BinpareApp::spawn_loaded_file(path));
                self.loading = true;
                self.split_mode = true;
                self.diff = None;
            }
        }

        // compute diff cache if both ready and diff not yet computed
        if self.file_a.is_some() && self.file_b.is_some() {
            let a_ready = self.file_a.as_ref().unwrap().ready.load(Ordering::Acquire);
            let b_ready = self.file_b.as_ref().unwrap().ready.load(Ordering::Acquire);
            if a_ready && b_ready && self.diff.is_none() {
                let a_data = self.file_a.as_ref().unwrap().data.lock().unwrap();
                let b_data = self.file_b.as_ref().unwrap().data.lock().unwrap();
                let max = std::cmp::max(a_data.len(), b_data.len());
                let mut diffv = vec![false; max];
                for i in 0..max {
                    if a_data.get(i).copied() != b_data.get(i).copied() {
                        diffv[i] = true;
                    }
                }
                // per-row indices
                let bytes_per_row = self.bytes_per_row;
                let rows = (max + bytes_per_row - 1) / bytes_per_row;
                let mut row_indices = Vec::new();
                for row in 0..rows {
                    let start = row * bytes_per_row;
                    let end = start + bytes_per_row;
                    let mut any = false;
                    for i in start..end {
                        if i < diffv.len() && diffv[i] { any = true; break; }
                    }
                    if any { row_indices.push(row); }
                }
                self.diff = Some(diffv);
                self.diff_row_indices = row_indices;
                self.current_diff_idx = None;
            }
        } else {
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
                                ui.push_id("pane_a", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{:08x}:", 0)).monospace());
                                        ui.add_space(6.0);
                                        let mut hex_header = String::with_capacity(self.bytes_per_row * 3);
                                        for i in 0..self.bytes_per_row { hex_header.push_str(&format!("{:02x} ", i)); }
                                        ui.add(egui::Label::new(RichText::new(hex_header).monospace().strong()));
                                        ui.separator();
                                        ui.label(RichText::new("ASCII").monospace().strong());
                                    });
                                    let selected_row = self.current_diff_idx.and_then(|i| self.diff_row_indices.get(i).copied());
                                    hexdump_ui(ui, &data, self.bytes_per_row, self.diff.as_ref(), self.diff_highlight, selected_row);
                                });
                            }
                        } else { ui.label("(no file)"); }
                    });

                    cols[1].vertical(|ui| {
                        if let Some(loaded) = &self.file_b {
                            let ready = loaded.ready.load(Ordering::Acquire);
                            let data = loaded.data.lock().unwrap();
                            if !ready {
                                ui.centered_and_justified(|ui| { ui.label("Loading file B..."); });
                            } else {
                                ui.push_id("pane_b", |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(format!("{:08x}:", 0)).monospace());
                                        ui.add_space(6.0);
                                        let mut hex_header = String::with_capacity(self.bytes_per_row * 3);
                                        for i in 0..self.bytes_per_row { hex_header.push_str(&format!("{:02x} ", i)); }
                                        ui.add(egui::Label::new(RichText::new(hex_header).monospace().strong()));
                                        ui.separator();
                                        ui.label(RichText::new("ASCII").monospace().strong());
                                    });
                                    let selected_row = self.current_diff_idx.and_then(|i| self.diff_row_indices.get(i).copied());
                                    hexdump_ui(ui, &data, self.bytes_per_row, self.diff.as_ref(), self.diff_highlight, selected_row);
                                });
                            }
                        } else { ui.label("(no file)"); }
                    });
                });
            } else {
                render_pane_single(ui, &self.file_a, self.bytes_per_row);
            }
        });
    }
}

fn render_pane_single(ui: &mut egui::Ui, file: &Option<LoadedFile>, bytes_per_row: usize) {
    ui.group(|ui| {
        if let Some(loaded) = file {
            let data = loaded.data.lock().unwrap();
            ui.push_id("single_pane", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{:08x}:", 0)).monospace());
                    ui.add_space(6.0);
                    let mut hex_header = String::with_capacity(bytes_per_row * 3);
                    for i in 0..bytes_per_row { hex_header.push_str(&format!("{:02x} ", i)); }
                    ui.add(egui::Label::new(RichText::new(hex_header).monospace().strong()));
                    ui.separator();
                    ui.label(RichText::new("ASCII").monospace().strong());
                });
                hexdump_ui(ui, &data, bytes_per_row, None, false, None);
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
                    if a != b { diffvec[i] = true; }
                }
                Some(diffvec)
            } else { None };
            hexdump_ui(ui, &data, bytes_per_row, diff.as_ref(), false, None);
        } else { ui.label("(no file)"); }
    });
}

fn hexdump_ui(ui: &mut egui::Ui, data: &[u8], bytes_per_row: usize, diff_opt: Option<&Vec<bool>>, diff_highlight: bool, selected_row: Option<usize>) {
    let rows = (data.len() + bytes_per_row - 1) / bytes_per_row;
    let row_height = 20.0;
    egui::ScrollArea::vertical().show_rows(ui, row_height, rows, |ui, row_range| {
        for row in row_range {
            let offset = row * bytes_per_row;
            // build strings
            let mut hex_line = String::with_capacity(bytes_per_row * 3);
            let mut ascii = String::with_capacity(bytes_per_row);
            let mut row_has_diff = false;
            let mut row_all_match = true;
            const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
            for i in 0..bytes_per_row {
                let idx = offset + i;
                if idx < data.len() {
                    let b = data[idx];
                    hex_line.push(HEX_CHARS[(b >> 4) as usize] as char);
                    hex_line.push(HEX_CHARS[(b & 0xF) as usize] as char);
                    hex_line.push(' ');
                    if b.is_ascii_graphic() || b == b' ' { ascii.push(b as char); } else { ascii.push('.'); }

                    if let Some(diffv) = diff_opt {
                        if idx < diffv.len() && diffv[idx] {
                            row_has_diff = true;
                            row_all_match = false;
                        }
                    }
                } else {
                    hex_line.push_str("   ");
                    ascii.push(' ');
                }
            }

            // decide background
            // Only show a background for differing rows when diff_highlight is enabled.
            // (Previously we painted green for every matching row when a diff vector
            // existed, which caused all rows to appear highlighted.)
            let mut bg_color: Option<Color32> = None;
            if diff_opt.is_some() {
                if row_has_diff && diff_highlight {
                    bg_color = Some(Color32::from_rgba_unmultiplied(220, 100, 100, 30));
                }
            }
            if let Some(sel) = selected_row { if sel == row { bg_color = Some(Color32::from_rgba_unmultiplied(255, 200, 50, 90)); }}

            // reserve rect and paint background
            let full_size = egui::vec2(ui.available_width(), row_height);
            let (rect, _resp) = ui.allocate_exact_size(full_size, egui::Sense::hover());
            let painter = ui.painter();
            if let Some(col) = bg_color { painter.rect_filled(rect, 0.0, col); }

            // Draw the whole row as a single text draw (avoids deprecated allocate_ui_at_rect/child APIs)
            let combined = format!("{:08x}:  {}  {}", offset, hex_line, ascii);
            let text_color = if bg_color.is_some() {
                Color32::BLACK
            } else if diff_opt.is_some() && row_all_match && diff_highlight {
                // only color matching rows when diff_highlight is enabled
                Color32::from_rgb(0, 100, 0)
            } else if row_has_diff && diff_highlight {
                Color32::from_rgb(150, 0, 0)
            } else {
                // default
                ui.visuals().text_color()
            };

            let font_id = egui::FontId::monospace(13.0);
            let pos = rect.min + egui::vec2(4.0, 2.0);
            painter.text(pos, egui::Align2::LEFT_TOP, combined, font_id, text_color);
        }
    });
}

fn main() {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "binpare",
        options,
        Box::new(|cc| Ok(Box::new(BinpareApp::new(cc)))),
    ).expect("failed to start eframe");
}

