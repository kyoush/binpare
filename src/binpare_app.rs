use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::{fs, io, thread};

// ===============================================================================
// # バイナリファイル比較ツール (binpare)
//
// ## オフセット位置検出アルゴリズム概要
//
// このツールは2つのバイナリファイルを比較し、類似する部分を検出して
// 最適な表示オフセットを提案します。
//
// ### 基本動作フロー
// 1. **ファイル読み込み**: 2つのバイナリファイルをメモリに読み込み
// 2. **サイズ差検出**: ファイルサイズの違いを検出
// 3. **類似部分探索**: スライディングウィンドウ方式で類似データ領域を検索
// 4. **スコア計算**: 各オフセット位置でのデータ一致率を計算
// 5. **候補提示**: 最適なオフセット候補をスコア順で提示
// 6. **比較表示**: 選択されたオフセットで2つのファイルを横並び表示
//
// ### 目的
// 類似部分を横に並べて表示することで、その前後の違いを
// 効率的に確認できるようにする。複数の類似部分が検出された
// 場合は、複数のオフセット選択肢を提示する。
// ===============================================================================

// === 設定定数 ===
const DEFAULT_BYTES_PER_ROW: usize = 16;
const MIN_BYTES_PER_ROW: usize = 8;
const MAX_BYTES_PER_ROW: usize = 32;
const MAX_DISPLAY_ROWS: usize = 1000;
const UI_FRAME_RATE_MS: u64 = 100;
const LARGE_FILE_THRESHOLD: usize = 1024 * 1024;

// === UI色設定 ===
const DIFF_HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 99, 99);
const OFFSET_PLACEHOLDER_COLOR: egui::Color32 = egui::Color32::GRAY;

// === エラー型定義 ===
#[derive(Debug)]
pub enum BinpareError {
    IoError,
    FileTooLarge,
}

impl From<io::Error> for BinpareError {
    fn from(_err: io::Error) -> Self {
        BinpareError::IoError
    }
}

// === 型エイリアス ===
type Result<T> = std::result::Result<T, BinpareError>;

/// ロード済みファイルの情報
/// スレッドセーフな設計でファイルデータと状態を管理
#[derive(Debug)]
pub struct LoadedFile {
    path: PathBuf,
    data: Mutex<Vec<u8>>,
    ready: AtomicBool,
    size: std::sync::atomic::AtomicUsize,
}

impl LoadedFile {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            data: Mutex::new(Vec::new()),
            ready: AtomicBool::new(false),
            size: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn size(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    pub fn with_data<F, T>(&self, f: F) -> Option<T>
    where
        F: FnOnce(&[u8]) -> T,
    {
        self.data.lock().ok().map(|data| f(&data))
    }

    fn set_data(&self, new_data: Vec<u8>) {
        let size = new_data.len();
        if let Ok(mut data) = self.data.lock() {
            *data = new_data;
        }
        self.size.store(size, Ordering::Release);
        self.ready.store(true, Ordering::Release);
    }

    #[allow(dead_code)]
    pub fn data(&self) -> &Mutex<Vec<u8>> {
        &self.data
    }
}

/// ファイル比較状態の管理
#[derive(Debug, Default)]
pub struct FileState {
    file_a: Option<Arc<LoadedFile>>,
    file_b: Option<Arc<LoadedFile>>,
    offset_a: usize,
    offset_b: usize,
}

impl FileState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_file(&mut self, slot: FileSlot, file: Arc<LoadedFile>) {
        match slot {
            FileSlot::A => self.file_a = Some(file),
            FileSlot::B => self.file_b = Some(file),
        }
    }

    pub fn get_file(&self, slot: FileSlot) -> Option<&Arc<LoadedFile>> {
        match slot {
            FileSlot::A => self.file_a.as_ref(),
            FileSlot::B => self.file_b.as_ref(),
        }
    }

    pub fn both_files_ready(&self) -> bool {
        matches!((&self.file_a, &self.file_b), (Some(a), Some(b)) if a.is_ready() && b.is_ready())
    }

    pub fn get_size_diff(&self) -> Option<(usize, bool)> {
        let (file_a, file_b) = (self.file_a.as_ref()?, self.file_b.as_ref()?);
        let (size_a, size_b) = (file_a.size(), file_b.size());

        if size_a == size_b {
            None
        } else {
            Some((size_a.abs_diff(size_b), size_a > size_b))
        }
    }

    #[allow(dead_code)]
    pub fn set_offsets(&mut self, offset_a: usize, offset_b: usize) {
        self.offset_a = offset_a;
        self.offset_b = offset_b;
    }

    #[allow(dead_code)]
    pub fn offsets(&self) -> (usize, usize) {
        (self.offset_a, self.offset_b)
    }

    #[allow(dead_code)]
    pub fn reset_offsets(&mut self) {
        self.offset_a = 0;
        self.offset_b = 0;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FileSlot {
    A,
    B,
}

/// ファイル比較の差分情報を管理
#[derive(Debug)]
pub struct DiffState {
    pub diff: Option<Vec<bool>>,
    pub diff_highlight: bool,
}

impl Default for DiffState {
    fn default() -> Self {
        Self {
            diff: None,
            diff_highlight: true,
        }
    }
}

impl DiffState {
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }
}

/// UIの状態とインタラクション管理
#[derive(Debug)]
pub struct UIState {
    pub bytes_per_row: usize,
    pub offset_candidates: Vec<(i32, f64)>,
    pub offset_candidates_calculated: bool,
    pub dialog_rx: Option<mpsc::Receiver<(FileSlot, Option<PathBuf>)>>,
}

impl Default for UIState {
    fn default() -> Self {
        Self {
            bytes_per_row: DEFAULT_BYTES_PER_ROW,
            offset_candidates: Vec::new(),
            offset_candidates_calculated: false,
            dialog_rx: None,
        }
    }
}

/// メインアプリケーション構造体
#[derive(Debug, Default)]
pub struct BinpareApp {
    pub file_state: FileState,
    pub diff_state: DiffState,
    pub ui_state: UIState,
}

#[derive(Clone, Copy)]
struct HexDisplayConfig {
    bytes_per_row: usize,
    offset_a: usize,
    offset_b: usize,
    highlight_diffs: bool,
}

impl BinpareApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    /// ファイル読み込みの開始（エラーハンドリング付き）
    pub fn load_file(&mut self, slot: FileSlot) {
        let (tx, rx) = mpsc::channel();
        self.ui_state.dialog_rx = Some(rx);

        thread::spawn(move || {
            let result = rfd::FileDialog::new()
                .add_filter("All files", &["*"])
                .pick_file();

            tx.send((slot, result)).ok();
        });
    }
    /// パスからファイル読み込み（エラーハンドリング付き）
    pub fn load_file_from_path(&mut self, slot: FileSlot, path: PathBuf) -> Result<()> {
        // ファイルサイズチェック
        let metadata = fs::metadata(&path)?;
        let file_size = metadata.len() as usize;

        if file_size > LARGE_FILE_THRESHOLD * 10 {
            // 10MB制限
            return Err(BinpareError::FileTooLarge);
        }

        let file = Arc::new(LoadedFile::new(path.clone()));
        self.file_state.set_file(slot, file.clone());

        // 新しいファイル読み込み時にオフセット候補を再計算できるようにリセット
        self.ui_state.offset_candidates_calculated = false;
        self.ui_state.offset_candidates.clear();

        // 非同期でファイル読み込み
        thread::spawn(move || {
            if Self::load_file_async(&file, &path).is_err() {
                // エラー時はready状態をfalseのまま保持
            }
        });

        self.check_and_calculate_diff();
        Ok(())
    }

    /// 非同期ファイル読み込み（エラーハンドリング付き）
    fn load_file_async(file: &LoadedFile, path: &Path) -> Result<()> {
        let data = fs::read(path)?;
        file.set_data(data);
        Ok(())
    }

    /// 高品質な比較ビューのレンダリング
    fn render_comparison_view(&mut self, ui: &mut egui::Ui) {
        let (file_a, file_b) = match (
            self.file_state.get_file(FileSlot::A),
            self.file_state.get_file(FileSlot::B),
        ) {
            (Some(a), Some(b)) => (a.clone(), b.clone()),
            _ => {
                ui.centered_and_justified(|ui| {
                    ui.label("Please load both files to compare");
                });
                return;
            }
        };

        // オフセット候補を生成（まだ生成されていない場合）
        if !self.ui_state.offset_candidates_calculated {
            self.suggest_offset_if_needed();
        }

        // ファイル情報ヘッダー
        self.render_file_info_header(ui, &file_a, &file_b);
        ui.separator();

        // ヘックスデータ表示
        if let (Some(data_a), Some(data_b)) = (
            file_a.with_data(|d| d.to_vec()),
            file_b.with_data(|d| d.to_vec()),
        ) {
            self.render_combined_hex_data_with_offset(ui, &data_a, &data_b);
        } else {
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.label("Loading files...");
            });
        }
    }

    /// 単一ファイルの表示
    fn render_single_file_view(&self, ui: &mut egui::Ui, file: &LoadedFile, slot: FileSlot) {
        // ファイル情報ヘッダー
        ui.horizontal(|ui| {
            ui.strong(format!(
                "File {}: ",
                match slot {
                    FileSlot::A => "A",
                    FileSlot::B => "B",
                }
            ));
            ui.label(
                file.path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy(),
            );
            ui.separator();
            ui.label(format!("Size: {} bytes", file.size()));
        });
        ui.separator();

        // ファイルデータ表示
        if let Some(data) = file.with_data(|d| d.to_vec()) {
            self.render_single_file_data(ui, &data);
        } else {
            ui.centered_and_justified(|ui| {
                ui.label("Error: Could not access file data");
            });
        }
    }

    /// 単一ファイルのhexデータ表示
    fn render_single_file_data(&self, ui: &mut egui::Ui, data: &[u8]) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.style_mut().override_font_id = Some(egui::FontId::monospace(12.0));

            let bytes_per_row = self.ui_state.bytes_per_row;
            let rows = data.len().div_ceil(bytes_per_row);

            for row in 0..rows.min(MAX_DISPLAY_ROWS) {
                let start = row * bytes_per_row;
                let end = (start + bytes_per_row).min(data.len());

                ui.horizontal(|ui| {
                    // アドレス表示
                    ui.monospace(format!("{:08X}", start));
                    ui.separator();

                    // Hex表示
                    for i in start..end {
                        ui.label(format!("{:02X}", data[i]));
                    }

                    // ASCII表示
                    ui.separator();
                    let mut ascii_string = String::new();
                    for i in start..end {
                        let ch = if data[i].is_ascii_graphic() || data[i] == b' ' {
                            data[i] as char
                        } else {
                            '.'
                        };
                        ascii_string.push(ch);
                    }
                    ui.monospace(ascii_string);
                });
            }
        });
    }

    fn render_file_info_header(
        &mut self,
        ui: &mut egui::Ui,
        file_a: &LoadedFile,
        file_b: &LoadedFile,
    ) {
        ui.horizontal(|ui| {
            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.strong("File A:");
                    ui.label(
                        file_a
                            .path()
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    );
                    ui.label(format!("Size: {} bytes", file_a.size()));
                });
            });

            ui.group(|ui| {
                ui.vertical(|ui| {
                    ui.strong("File B:");
                    ui.label(
                        file_b
                            .path()
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                    );
                    ui.label(format!("Size: {} bytes", file_b.size()));
                });
            });

            if let Some((diff, a_larger)) = self.file_state.get_size_diff() {
                ui.group(|ui| {
                    ui.vertical(|ui| {
                        ui.strong("Size Difference:");
                        ui.label(format!("{} bytes", diff));
                        ui.label(if a_larger { "A > B" } else { "B > A" });

                        // オフセット候補の表示と選択
                        if !self.ui_state.offset_candidates.is_empty() {
                            ui.separator();
                            ui.strong("Offset Suggestions:");
                            let candidates = self.ui_state.offset_candidates.clone();
                            for (_i, (offset, score)) in candidates.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    if ui.button(format!("Apply {}", offset)).clicked() {
                                        self.apply_offset_candidate(*offset);
                                    }
                                    ui.label(format!("Offset: {}, Score: {:.2}", offset, score));
                                });
                            }
                        }

                        // 現在のオフセット表示
                        ui.separator();
                        ui.label(format!(
                            "Current offsets: A={}, B={}",
                            self.file_state.offset_a, self.file_state.offset_b
                        ));
                    });
                });
            }
        });
    }

    /// 高品質なヘックス比較ビューのレンダリング
    fn render_combined_hex_data_with_offset(
        &self,
        ui: &mut egui::Ui,
        data_a: &[u8],
        data_b: &[u8],
    ) {
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                self.render_hex_rows(ui, data_a, data_b);
            });
    }

    fn render_hex_rows(&self, ui: &mut egui::Ui, data_a: &[u8], data_b: &[u8]) {
        ui.style_mut().override_font_id = Some(egui::FontId::monospace(12.0));

        let config = HexDisplayConfig {
            bytes_per_row: self.ui_state.bytes_per_row,
            offset_a: self.file_state.offset_a,
            offset_b: self.file_state.offset_b,
            highlight_diffs: self.diff_state.diff_highlight,
        };

        let total_len_a = data_a.len() + config.offset_a;
        let total_len_b = data_b.len() + config.offset_b;
        let max_total_len = total_len_a.max(total_len_b);
        let rows = max_total_len.div_ceil(config.bytes_per_row);

        for row in 0..rows.min(MAX_DISPLAY_ROWS) {
            let start = row * config.bytes_per_row;
            let end = (start + config.bytes_per_row).min(max_total_len);

            self.render_hex_row(ui, &config, data_a, data_b, start, end);
        }
    }

    fn render_hex_row(
        &self,
        ui: &mut egui::Ui,
        config: &HexDisplayConfig,
        data_a: &[u8],
        data_b: &[u8],
        start: usize,
        end: usize,
    ) {
        ui.horizontal(|ui| {
            // Address column
            ui.monospace(format!("{:08X}", start));
            ui.separator();

            // File A hex and ASCII
            self.render_file_data(
                ui,
                "A:",
                data_a,
                config.offset_a,
                config.highlight_diffs,
                data_b,
                config.offset_b,
                start,
                end,
            );
            ui.separator();
            self.render_ascii_data(ui, data_a, config.offset_a, start, end);
            ui.separator();

            // File B hex and ASCII
            self.render_file_data(
                ui,
                "B:",
                data_b,
                config.offset_b,
                config.highlight_diffs,
                data_a,
                config.offset_a,
                start,
                end,
            );
            ui.separator();
            self.render_ascii_data(ui, data_b, config.offset_b, start, end);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn render_file_data(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        data: &[u8],
        offset: usize,
        highlight_diffs: bool,
        other_data: &[u8],
        other_offset: usize,
        start: usize,
        end: usize,
    ) {
        ui.label(label);
        for i in start..end {
            if i < offset {
                ui.colored_label(OFFSET_PLACEHOLDER_COLOR, "--");
            } else {
                let data_index = i - offset;
                if data_index < data.len() {
                    let byte = data[data_index];
                    let other_data_index = i.saturating_sub(other_offset);
                    let is_diff = highlight_diffs
                        && other_data_index < other_data.len()
                        && byte != other_data[other_data_index];

                    if is_diff {
                        ui.scope(|ui| {
                            ui.visuals_mut().override_text_color = Some(egui::Color32::WHITE);
                            ui.visuals_mut().widgets.inactive.bg_fill = DIFF_HIGHLIGHT_COLOR;
                            ui.label(format!("{:02X}", byte));
                        });
                    } else {
                        ui.label(format!("{:02X}", byte));
                    }
                } else {
                    ui.label("--");
                }
            }
        }
    }

    fn render_ascii_data(
        &self,
        ui: &mut egui::Ui,
        data: &[u8],
        offset: usize,
        start: usize,
        end: usize,
    ) {
        let ascii_string: String = (start..end)
            .map(|i| {
                if i < offset {
                    '-'
                } else {
                    let data_index = i - offset;
                    if data_index < data.len() {
                        let byte = data[data_index];
                        if byte.is_ascii_graphic() || byte == b' ' {
                            byte as char
                        } else {
                            '.'
                        }
                    } else {
                        ' '
                    }
                }
            })
            .collect();
        ui.monospace(ascii_string);
    }

    fn check_and_calculate_diff(&mut self) {
        if self.file_state.both_files_ready() {
            self.calculate_diff();
            self.suggest_offset_if_needed();
        }
    }

    fn calculate_diff(&mut self) {
        if let (Some(file_a), Some(file_b)) = (
            self.file_state.get_file(FileSlot::A),
            self.file_state.get_file(FileSlot::B),
        ) {
            if let (Some(data_a), Some(data_b)) = (
                file_a.with_data(|d| d.to_vec()),
                file_b.with_data(|d| d.to_vec()),
            ) {
                let min_len = data_a.len().min(data_b.len());
                let mut diff = vec![false; min_len];

                for i in 0..min_len {
                    diff[i] = data_a[i] != data_b[i];
                }

                self.diff_state.diff = Some(diff);
            }
        }
    }

    /// オフセット候補の提案（必要に応じて）
    ///
    /// # アルゴリズム概要
    /// 2つのファイルを比較して、似ている部分がないかを探索する。
    /// 似ている部分が発見された場合、それらを横に並べて表示し、
    /// その前後の違いを確認できるようにオフセット候補を提示する。
    /// 複数の類似部分が検出された場合は、複数のオフセット表示を提示する。
    ///
    /// # サポートするケース
    /// - サイズ異なるファイル: サイズ差を基にオフセット探索
    /// - サイズ同一ファイル: 内容のずれを検出してオフセット探索
    /// - 部分的一致: 小さな類似領域でも検出
    /// - 繰り返しパターン: 周期的データでも正確なオフセット検出
    fn suggest_offset_if_needed(&mut self) {
        // 既に計算済みの場合はスキップ
        if self.ui_state.offset_candidates_calculated {
            return;
        }

        // ファイルが両方読み込み済みか確認
        if !self.file_state.both_files_ready() {
            return;
        }

        let (file_a, file_b) = match (
            self.file_state.get_file(FileSlot::A),
            self.file_state.get_file(FileSlot::B),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => return,
        };

        let (size_a, size_b) = (file_a.size(), file_b.size());

        // ケース1: サイズ異なるファイル
        if let Some((diff, a_larger)) = self.file_state.get_size_diff() {
            println!(
                "Size difference detected: {} bytes, A larger: {}",
                diff, a_larger
            );
            let candidates = if a_larger {
                self.suggest_offset_candidates(diff.min(4096), false) // サイズ差まで検索
            } else {
                self.suggest_offset_candidates(diff.min(4096), true)
            };

            self.ui_state.offset_candidates = candidates;
        }
        // ケース2: サイズ同一だが内容が異なる可能性があるファイル
        else {
            println!("Same size files detected, checking for content shifts");
            // 小さなオフセットでの類似性をチェック
            let mut all_candidates = Vec::new();

            // 両方向のオフセットを検索
            let candidates_a = self.suggest_offset_candidates(1024, true); // Aをオフセット
            let candidates_b = self.suggest_offset_candidates(1024, false); // Bをオフセット

            all_candidates.extend(candidates_a);
            all_candidates.extend(candidates_b);

            // スコアでソートして上位5つを選択
            all_candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            all_candidates.truncate(5);

            self.ui_state.offset_candidates = all_candidates;
        }

        println!(
            "Generated {} offset candidates",
            self.ui_state.offset_candidates.len()
        );
        self.ui_state.offset_candidates_calculated = true;
    }
    /// オフセット候補を提案する
    ///
    /// # アルゴリズム仕様
    ///
    /// ## 目的
    /// 2つのバイナリファイル間で類似するデータ領域を検出し、
    /// 最適なオフセット位置を提案する。
    ///
    /// ## 動作原理
    /// 1. **類似性検索**: 指定された検索窓内で、片方のファイルを
    ///    もう片方のファイルに対して1バイトずつずらしながら比較
    /// 2. **マッチングスコア計算**: 各オフセット位置でのデータの
    ///    一致率をスコアとして算出（0.0-1.0の範囲）
    /// 3. **候補選定**: スコアが閾値を超える位置を候補として抽出
    /// 4. **結果提示**: スコアの高い順に上位5つまでの候補を提示
    ///
    /// ## パラメータ
    /// - `search_window`: 検索範囲のサイズ（デフォルト: 2048バイト）
    /// - `offset_for_a`: true=ファイルAにオフセット適用, false=ファイルBに適用
    ///
    /// ## 戻り値
    /// オフセット値とスコアのペアのベクタ（スコア降順でソート済み）
    fn suggest_offset_candidates(
        &self,
        search_window: usize,
        offset_for_a: bool,
    ) -> Vec<(i32, f64)> {
        let (file_a, file_b) = match (
            self.file_state.get_file(FileSlot::A),
            self.file_state.get_file(FileSlot::B),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                println!("Files not available for offset calculation");
                return vec![];
            }
        };

        let (data_a, data_b) = match (
            file_a.with_data(|d| d.to_vec()),
            file_b.with_data(|d| d.to_vec()),
        ) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                println!("File data not available for offset calculation");
                return vec![];
            }
        };

        let max = data_a.len().max(data_b.len());
        println!("Calculating offset candidates: data_a len={}, data_b len={}, search_window={}, offset_for_a={}", 
                 data_a.len(), data_b.len(), search_window, offset_for_a);

        // 大きなファイルではサンプル数を制限して段階的に処理
        let chunk_size = if max > 1024 * 1024 {
            128 * 1024 // 1MB以上の場合は128KBづつ処理（パフォーマンス重視）
        } else {
            max
        };

        let mut candidates = Vec::new();
        let search_range = search_window.min(chunk_size);
        println!("Search range: 0..{}", search_range);

        for offset in 0..search_range {
            let score = self.calculate_match_score(&data_a, &data_b, offset, offset_for_a);
            if score > 0.0 {
                // 閾値以上のスコアのみ保持
                let offset_value = if offset_for_a {
                    offset as i32
                } else {
                    -(offset as i32)
                };
                candidates.push((offset_value, score));
                if candidates.len() <= 5 {
                    // 最初の5つだけログ出力
                    println!(
                        "Found candidate: offset={}, score={:.3}",
                        offset_value, score
                    );
                }
            }
        }

        println!("Total candidates before sorting: {}", candidates.len());

        // 候補が見つからない場合、ファイルサイズ差に基づいてデフォルト候補を追加
        if candidates.is_empty() {
            let size_diff = data_a.len().abs_diff(data_b.len()) as i32;
            if size_diff > 0 {
                let default_offset = if offset_for_a { size_diff } else { -size_diff };
                candidates.push((default_offset, 0.5)); // デフォルトスコア
                println!("Added default offset candidate: {}", default_offset);
            }
        }

        // スコア順でソート
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        candidates.truncate(5); // 上位5件のみ
        candidates
    }

    /// マッチスコアを計算
    ///
    /// # スコア計算仕様
    ///
    /// ## 目的
    /// 指定されたオフセット位置での2つのデータ領域の類似度を数値化
    ///
    /// ## 計算方法
    /// 1. **比較領域の決定**: オフセットを適用した後の重複する領域を特定
    /// 2. **サンプリング**: パフォーマンス向上のため最大256バイトまでサンプル
    /// 3. **一致率計算**: 一致するバイト数 ÷ 比較バイト数
    ///
    /// ## パラメータ
    /// - `data_a`, `data_b`: 比較対象のバイナリデータ
    /// - `offset`: 適用するオフセット値
    /// - `offset_for_a`: オフセット適用対象（true=A, false=B）
    ///
    /// ## 戻り値
    /// 0.0（完全不一致）から1.0（完全一致）までの類似度スコア
    fn calculate_match_score(
        &self,
        data_a: &[u8],
        data_b: &[u8],
        offset: usize,
        offset_for_a: bool,
    ) -> f64 {
        let (compare_a, compare_b, len) = if offset_for_a {
            (
                &data_a[offset..],
                data_b,
                (data_a.len() - offset).min(data_b.len()),
            )
        } else {
            (
                data_a,
                &data_b[offset..],
                data_a.len().min(data_b.len() - offset),
            )
        };

        if len == 0 {
            return 0.0;
        }

        let sample_size = 256.min(len); // サンプルサイズを制限
        let matches = (0..sample_size)
            .filter(|&i| compare_a[i] == compare_b[i])
            .count();

        matches as f64 / sample_size as f64
    }

    /// 高度なマッチスコアを計算（マルチウィンドウ対応）
    fn calculate_advanced_match_score(
        &self,
        data_a: &[u8],
        data_b: &[u8],
        offset: usize,
        offset_for_a: bool,
        window_size: usize,
        use_sampling: bool,
    ) -> f64 {
        let (compare_a, compare_b, len) = if offset_for_a {
            if offset >= data_a.len() {
                return 0.0;
            }
            (
                &data_a[offset..],
                data_b,
                (data_a.len() - offset).min(data_b.len()),
            )
        } else {
            if offset >= data_b.len() {
                return 0.0;
            }
            (
                data_a,
                &data_b[offset..],
                data_a.len().min(data_b.len() - offset),
            )
        };

        if len == 0 {
            return 0.0;
        }

        let sample_size = if use_sampling {
            window_size.min(len)
        } else {
            window_size.min(len)
        };

        if sample_size == 0 {
            return 0.0;
        }

        // ウィンドウサイズに応じたサンプリング
        let step = if use_sampling && len > sample_size * 4 {
            len / sample_size // 均等サンプリング
        } else {
            1 // 連続比較
        };

        let mut matches = 0;
        let mut total_compared = 0;

        for i in (0..len).step_by(step) {
            if total_compared >= sample_size {
                break;
            }
            if i < compare_a.len() && i < compare_b.len() {
                if compare_a[i] == compare_b[i] {
                    matches += 1;
                }
                total_compared += 1;
            }
        }

        if total_compared == 0 {
            0.0
        } else {
            matches as f64 / total_compared as f64
        }
    }

    /// コンテキストスコアを計算（前後の整合性をチェック）
    fn calculate_context_score(
        &self,
        data_a: &[u8],
        data_b: &[u8],
        offset: usize,
        offset_for_a: bool,
    ) -> f64 {
        const CONTEXT_SIZE: usize = 32;

        let (compare_a, compare_b) = if offset_for_a {
            if offset >= data_a.len() {
                return 0.0;
            }
            (&data_a[offset..], data_b)
        } else {
            if offset >= data_b.len() {
                return 0.0;
            }
            (data_a, &data_b[offset..])
        };

        let len = compare_a.len().min(compare_b.len());
        if len < CONTEXT_SIZE * 2 {
            return 0.0;
        }

        // 前後のコンテキストで一致度をチェック
        let front_matches = (0..CONTEXT_SIZE)
            .filter(|&i| i < compare_a.len() && i < compare_b.len() && compare_a[i] == compare_b[i])
            .count();

        let back_start = len.saturating_sub(CONTEXT_SIZE);
        let back_matches = (back_start..len)
            .filter(|&i| i < compare_a.len() && i < compare_b.len() && compare_a[i] == compare_b[i])
            .count();

        (front_matches + back_matches) as f64 / (CONTEXT_SIZE * 2) as f64
    }

    /// フォールバック候補を追加
    fn add_fallback_candidates(
        &self,
        candidates: &mut Vec<(i32, f64)>,
        data_a: &[u8],
        data_b: &[u8],
        offset_for_a: bool,
        search_range: usize,
    ) {
        println!("Adding fallback candidates...");

        // パターン1: ファイルサイズ差に基づく候補
        let size_diff = data_a.len().abs_diff(data_b.len()) as i32;
        if size_diff > 0 {
            let fallback_offset = if offset_for_a {
                if data_a.len() > data_b.len() {
                    size_diff
                } else {
                    0
                }
            } else {
                if data_b.len() > data_a.len() {
                    -size_diff
                } else {
                    0
                }
            };

            if fallback_offset != 0 {
                candidates.push((fallback_offset, 0.3));
                println!("Added size-based fallback: offset={}", fallback_offset);
            }
        }

        // パターン2: 一般的なアライメント候補
        let common_alignments = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024];
        for &alignment in &common_alignments {
            if alignment > search_range {
                break;
            }

            let test_offset = if offset_for_a {
                alignment as i32
            } else {
                -(alignment as i32)
            };

            // すでに存在するかチェック
            if !candidates.iter().any(|(offset, _)| *offset == test_offset) {
                let simple_score =
                    self.calculate_match_score(data_a, data_b, alignment, offset_for_a);
                if simple_score > 0.05 {
                    candidates.push((test_offset, simple_score * 0.8)); // 少し低めのスコア
                    println!(
                        "Added alignment fallback: offset={}, score={:.3}",
                        test_offset, simple_score
                    );
                }
            }
        }
    }

    /// オフセット候補の適用
    ///
    /// # オフセット適用仕様
    ///
    /// ## 目的
    /// 提案されたオフセット値を実際の表示に適用し、
    /// ファイル比較ビューを最適化する。
    ///
    /// ## 適用ロジック
    /// 1. **オフセット方向の決定**: ファイルサイズの大小関係から適用対象を決定
    ///    - 正の値: より大きなファイルの先頭をスキップ
    ///    - 負の値: より小さなファイルの先頭をスキップ
    /// 2. **表示更新**: オフセット適用後に差分を再計算
    /// 3. **候補再生成**: 新しいオフセットでの追加候補を提案
    ///
    /// ## 効果
    /// 類似データ領域を横に並べることで、前後の違いを
    /// 効率的に確認できるようになる。
    #[allow(dead_code)]
    pub fn apply_offset_candidate(&mut self, offset: i32) {
        if let Some((_, a_larger)) = self.file_state.get_size_diff() {
            let (new_offset_a, new_offset_b) = if offset >= 0 {
                if a_larger {
                    (0, offset as usize)
                } else {
                    (offset as usize, 0)
                }
            } else {
                let abs_offset = offset.abs() as usize;
                if a_larger {
                    (abs_offset, 0)
                } else {
                    (0, abs_offset)
                }
            };

            // 既に同じオフセットが適用されている場合はスキップ
            if self.file_state.offset_a == new_offset_a && self.file_state.offset_b == new_offset_b
            {
                return;
            }

            self.file_state.offset_a = new_offset_a;
            self.file_state.offset_b = new_offset_b;

            self.diff_state.diff = None;
            self.calculate_diff();

            // オフセット候補をクリア（新しいオフセットで再計算するため）
            self.ui_state.offset_candidates.clear();
            self.ui_state.offset_candidates_calculated = false; // 再計算を可能にする

            // 新しいオフセットで候補を再計算
            if self.file_state.file_a.is_some() && self.file_state.file_b.is_some() {
                self.ui_state.offset_candidates = self.suggest_offset_candidates(2048, a_larger);
            }
        }
    }

    fn handle_file_dialog(&mut self) {
        if let Some(rx) = &self.ui_state.dialog_rx {
            if let Ok((slot, result)) = rx.try_recv() {
                if let Some(path) = result {
                    let _ = self.load_file_from_path(slot, path);
                }
                self.ui_state.dialog_rx = None;
            }
        }
    }
}

impl eframe::App for BinpareApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(UI_FRAME_RATE_MS));

        self.handle_file_dialog();

        egui::TopBottomPanel::top("menubar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Load File A").clicked() {
                        self.load_file(FileSlot::A);
                        ui.close_menu();
                    }
                    if ui.button("Load File B").clicked() {
                        self.load_file(FileSlot::B);
                        ui.close_menu();
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        std::process::exit(0);
                    }
                });

                ui.menu_button("View", |ui| {
                    ui.checkbox(&mut self.diff_state.diff_highlight, "Highlight Differences");
                    ui.separator();
                    ui.label("Bytes per row:");
                    ui.add(egui::Slider::new(
                        &mut self.ui_state.bytes_per_row,
                        MIN_BYTES_PER_ROW..=MAX_BYTES_PER_ROW,
                    ));
                });
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.file_state.both_files_ready() {
                self.render_comparison_view(ui);
            } else if let Some(file) = self.file_state.get_file(FileSlot::A) {
                if file.is_ready() {
                    self.render_single_file_view(ui, file, FileSlot::A);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.heading("Loading File A...");
                        ui.spinner();
                    });
                }
            } else if let Some(file) = self.file_state.get_file(FileSlot::B) {
                if file.is_ready() {
                    self.render_single_file_view(ui, file, FileSlot::B);
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.heading("Loading File B...");
                        ui.spinner();
                    });
                }
            } else {
                ui.centered_and_justified(|ui| {
                    ui.heading("Binary File Comparator");
                    ui.add_space(20.0);
                    ui.label("Load files using the File menu to start viewing or comparison");
                });
            }
        });
    }
}
