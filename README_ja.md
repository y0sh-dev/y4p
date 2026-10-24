
<div align="center">

# y4p

**Wayland統合型クリップボード・インフラストラクチャ**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Wayland-lightgreen.svg)](https://wayland.freedesktop.org)
[![Version](https://img.shields.io/badge/version-0.7.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` は、Wayland向けにネイティブ設計された、スタンドアロンのクリップボードマネージャーです。

デーモンはひとつだけ。監視用と提供用でプロセスを分ける必要はありません。単一のバイナリが、クリップボードの監視、履歴の復元、そして永続化までを担います。
データを失わない厳格な整合性と、多層防御のリソース制御を備え、テキスト、バイナリ、高解像度の画像までを一定の性能で扱います。

</div>

---

## 動作環境

`y4p` が対象とするのは **`ext-data-control-v1` プロトコルをネイティブに実装したコンポジタ** です。

- **対応**: wlroots系のコンポジタ（Sway、Hyprland、river、およびその派生環境）。
- **非対応**: `ext-data-control-v1` を実装していないコンポジタ（標準のGNOME/Mutter、KWinなど）。

X11向けの互換レイヤーではなく、このプロトコルに直接依存しています。これは意図的な設計上の選択であり、手抜きではありません。お使いの環境で対応しているか不安な場合は、インストール前にコンポジタのWaylandプロトコル一覧をご確認ください。

---

## 特長

- **統合デーモン**: 監視・IPC応答・ディスクへの永続化を、単一プロセスで完結させます。外部のデーモン管理は不要です。
- **ハイブリッドストレージ**: テキストとメタデータは、高速検索可能なSQLiteへ。スクリーンショットやGIFなどの大きなバイナリは、内容アドレス（ハッシュ）による重複排除付きのファイルシステムキャッシュへ。両者は互いの領分を侵しません。70MBを超えるアイテムでも同様です。
- **スクリプトファーストなCLI**: すべてのレコードには、表示位置とは独立した不変のIDが割り当てられます。未定義のフラグを即座に拒絶する厳格なパーサと組み合わせることで、`fzf` や `rofi`、シェルパイプラインの中でも予測可能に振る舞います。
- **ゼロロス整合性とPin保護**: Pinを立てたレコードには、履歴のローテーションが決して触れません。スキーマのマイグレーションも段階的かつ冪等に設計されているため、アップデートを挟んでも既存の履歴が書き換わったり失われたりすることはありません。
- **ローカル画像パイプライン**: 画像はメタデータ（EXIF・GPSなど）を除去して再エンコードされます。分離されたスクラッチディレクトリ、プロセスグループ単位のタイムアウト監視、リソース上限、同時実行数の制限、自己回復型のサーキットブレーカーで保護されています。失敗時は常に元のバイト列をそのまま保存します。
- **構造的な堅牢性**: パニックしない実行パス（`unwrap`・`expect`・`panic!` はワークスペース全体のlintで禁止）と、意図的に小さく固定された依存関係。

---

## コマンド一覧

どんなことができるツールか、ざっと把握するための一覧です。詳細なフラグや使用例は `y4p help` を参照してください。

| コマンド | 概要 |
| :--- | :--- |
| `daemon` | バックグラウンド監視とIPCリスナーを開始します。 |
| `list` / `search` | クリップボード履歴を閲覧・検索します。 |
| `copy-to` | 履歴のエントリを現在のクリップボードへ復元します。 |
| `show` | レコードの中身を直接確認します。 |
| `store` / `paste-from` | 標準入力から手動で取り込む、またはOSクリップボードを直接読み出します。 |
| `pin` / `unpin` | レコードを自動ローテーションから保護・解除します。 |
| `delete` / `wipe` | 1件を削除する、または履歴を全消去します。 |
| `status` / `pause` / `resume` | デーモンの監視状態を確認・制御します。 |

---

## クイックスタート

まずビルドします。

```bash
git clone https://github.com/y0sh-dev/y4p.git
cd y4p
cargo build --release
sudo cp target/release/y4p /usr/local/bin/
```

デーモンを起動します。

```bash
y4p daemon &
```

動作を確認します。

```bash
y4p status
echo "Wayland native clipboard" | y4p store
y4p list
```

Zsh補完を有効にする場合（任意）。`compinit` の実行前に、補完ディレクトリを `fpath` に追加してください。

```zsh
fpath+=(/path/to/y4p/completions)
```

各コマンドの詳細なフラグや使用例は、以下で確認できます。

```bash
y4p help
```

<details>
<summary><strong>検証とテスト</strong></summary>

```bash
# ワークスペース全体のユニットテスト
cargo test --workspace

# 厳格なlintチェック
cargo clippy --workspace --all-targets -- -D warnings

# 上記2つをまとめた自動検証スクリプト
bash scripts/check.sh
```

</details>

---

## 設定

設定なしでも、妥当な既定値でそのまま動作します。

履歴の保持件数やMIMEフィルタなどを変更する場合は、`$XDG_CONFIG_HOME/y4p/y4p.toml`（未設定なら `~/.config/y4p/y4p.toml`）に設定ファイルを置いてください。設定ファイルが無い、または壊れていても、デーモンは既定値で起動します。

解説付きのひな形が [`y4p.toml.example`](y4p.toml.example) にあります。

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/y4p"
cp y4p.toml.example "${XDG_CONFIG_HOME:-$HOME/.config}/y4p/y4p.toml"
```

---

## アーキテクチャ概要

`y4p` はCargoワークスペースです。ルートのパッケージは薄い組み立て役（`src/main.rs`）に徹し、各ドメインは専用のクレートに分かれています。依存関係は常に `y4p-core` に向かって一方向に流れ、循環はコンパイラが拒否します。

- **[`crates/core`](crates/core)（`y4p-core`）**: 定数、XDGパス、`y4p.toml` パーサ、MIMEユーティリティ、`Payload` の語彙、`splice(2)`・パイプ容量・FastCDC のプリミティブ、終了フラグ。
- **[`crates/storage`](crates/storage)（`y4p-storage`）**: SQLiteストア、スキーマのマイグレーション、重複排除付きチャンク保存、ファイルシステム上のBLOBキャッシュ。
- **[`crates/image`](crates/image)（`y4p-image`）**: 画像の判別、サンドボックス化された変換、サーキットブレーカー、同時実行制限。
- **[`crates/wayland`](crates/wayland)（`y4p-wayland`）**: `ext-data-control-v1` のハンドラ、ストリーミングI/O、上限付きの取り込みキュー。
- **[`crates/daemon`](crates/daemon)（`y4p-daemon`）**: イベントループ、IPCソケット、バックグラウンドワーカー（画像変換・ハッシュ計算・永続化）。
- **[`crates/cli`](crates/cli)（`y4p-cli`）**: 厳格な引数解析、コマンドハンドラ、出力整形。

---

## ドキュメント

このREADMEは、あえて短くまとめた入門ガイドです。

デーモンがなぜ単一スレッドでI/Oを多重化するのか、Pin保護が履歴ローテーションとどう連携するのか、不変IDがなぜ必要なのか——そうした内部設計の「なぜ」は、`docs/` に技術文書としてまとめています（英語）。入口は [`docs/00_overview.md`](docs/00_overview.md) です。ワークスペースの構成、クレートの責務、依存の向き、取り込みのストリーム契約を扱い、`docs/` 全体への案内も兼ねています。

プロジェクトの運営・計画・参照文書:

- [`.github/AI_POLICY.md`](.github/AI_POLICY.md): 生成AI（LLM）の利用に関する方針（英語）。
- [`PERFORMANCE.md`](PERFORMANCE.md): 性能の定量ベースライン（レイテンシ、アロケーション、メモリ上限）とゼロコピー期の目標（英語）。再現は `cargo bench -p y4p-daemon --bench baseline`。
- [`ROADMAP_v0.8.md`](ROADMAP_v0.8.md): v0.8.0 へ向かう v0.7.x ゼロコピーのロードマップ（英語）。
- [`README.md`](README.md): 英語版README。

---

## ライセンス

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)
