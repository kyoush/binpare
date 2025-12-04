# Binpare 差分表示仕様書

## 概要

現在実装されている差分検出・表示機能の詳細仕様です。

## 実装済み機能

### 差分検出アルゴリズム

- **バイト単位比較**: 2 つのファイルを同一インデックスでバイト比較
- **ファイルサイズ差対応**: 長さが異なる場合も適切に処理
- **効率的な実装**: `is_diff_at_index()` でリアルタイム判定

### 差分表示

- **視覚的ハイライト**: 差分バイトを赤背景+白文字で強調
- **切り替え可能**: メニューから"Highlight Differences"で ON/OFF
- **同期表示**: A/B ファイルの対応する位置を同時にハイライト

### UI 配置

```
Address | A: [hex] [ASCII] | B: [hex] [ASCII]
```

- 標準的な hexdump レイアウト
- 横並び配置で A/B が明確に分離
- 各ファイルの hex と ASCII が隣接配置

## 技術実装

### コア関数

```rust
fn is_diff_at_index(&self, data_a: &[u8], data_b: &[u8], index: usize) -> bool {
    if index < data_a.len() && index < data_b.len() {
        data_a[index] != data_b[index]
    } else {
        data_a.len() != data_b.len() // サイズ差も差分扱い
    }
}
```

### ハイライト描画

```rust
if is_diff && self.diff_state.diff_highlight {
    ui.scope(|ui| {
        ui.visuals_mut().override_text_color = Some(egui::Color32::WHITE);
        ui.visuals_mut().widgets.inactive.bg_fill = DIFF_HIGHLIGHT_COLOR;
        ui.label(format!("{:02X}", byte));
    });
}
```

## 設定と定数

```rust
const DIFF_HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 99, 99);
const OFFSET_PLACEHOLDER_COLOR: egui::Color32 = egui::Color32::GRAY;
```

## 使用方法

1. 2 つのファイルを読み込み（File A, File B）
2. メニューから"View > Highlight Differences"をチェック
3. 差分バイトが自動的に赤色でハイライト表示

## パフォーマンス特性

- **リアルタイム計算**: スクロール時に動的に差分判定
- **メモリ効率**: 差分データをキャッシュしない軽量実装
- **高速描画**: egui::scope による局所的スタイル変更

## 制限事項

- **行数制限**: 最大 1000 行まで表示（MAX_DISPLAY_ROWS）
- **ファイルサイズ**: 10MB 制限（LARGE_FILE_THRESHOLD）
- **表示粒度**: バイト単位のみ（行単位ハイライトなし）

## 将来的拡張

- 差分統計情報の表示
- 差分箇所へのジャンプ機能
- カスタムハイライト色
- 差分範囲の選択・コピー
