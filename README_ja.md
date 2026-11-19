
<div align="center">

# y4p

**Wayland統合型クリップボード・インフラストラクチャ**

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-Wayland-lightgreen.svg)](https://wayland.freedesktop.org)
[![Version](https://img.shields.io/badge/version-0.9.0-green.svg)](https://github.com/y0sh-dev/y4p/releases/latest)

`y4p` は、Wayland向けにネイティブ設計された、スタンドアロンのクリップボードマネージャーです。

デーモンはひとつだけ。監視用と提供用でプロセスを分ける必要はありません。単一のバイナリが、クリップボードの監視、履歴の復元、そして永続化までを担います。
データを失わない厳格な整合性と、多層防御のリソース制御を備え、テキスト、バイナリ、高解像度の画像までを固定されたメモリ・ディスク上限のもとで扱います。

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
- **ハイブリッドストレージ**: メタデータとテキストは高速検索可能なSQLiteへ。大きなバイナリは、ペイロード全体のSHA3ハッシュによる重複排除付きで、内容アドレス方式のキャッシュディレクトリへ格納します。キャッシュ総量は512 MiBのLRUクォータで有界化されます。
- **既定は無加工保存**: コピーされた画像はそのままのバイト列で保存します。メタデータ除去を伴う画像サニタイズは明示的なオプトイン（`[image] sanitize = true`）であり、その場合にのみImageMagickを必要とします。
- **スクリプトファーストなCLI**: すべてのレコードには、表示位置とは独立した不変のIDが割り当てられます。未定義のフラグを即座に拒絶する厳格なパーサと組み合わせることで、`fzf` や `rofi`、シェルパイプラインの中でも予測可能に振る舞います。
- **ゼロロス整合性とPin保護**: Pinを立てたレコードには、履歴のローテーションもキャッシュ退避も決して触れません。スキーマのマイグレーションは段階的かつ冪等に設計されているため、アップデートを挟んでも既存の履歴が書き換わったり失われたりすることはありません。
- **構造的な堅牢性**: 常駐層はパニックしない実行パス（`unwrap`・`expect`・`panic!` はlintで禁止）で構成され、依存関係は意図的に小さく固定されています。
- **組み込みの可観測性**: `y4p stats` がエントリ数・ディスク占有・デーモンのシェッド回数を、`y4p doctor` が環境・コンポジタ・DB・ソケット・クォータを `[PASS]` / `[WARN]` / `[FAIL]` で点検します。

---

## コマンド一覧

どんなことができるツールか、ざっと把握するための一覧です。詳細なフラグや使用例は `y4p help` を参照してください。

| コマンド | 概要 |
| :--- | :--- |
| `daemon` | バックグラウンド監視とIPCリスナーを開始します。 |
| `list` / `search` | クリップボード履歴を閲覧・検索します。 |
| `copy-to` | 履歴のエントリを現在のクリップボードへ復元します。 |
| `show` | レコードの中身を直接確認します。 |
| `store` | 標準入力から手動で取り込みます。 |
| `paste-from [-l\|--list-types \| mime]` | OSクリップボードを直接読み出します。`-l` は広告されているMIMEタイプの一覧のみを出力します。 |
| `pin` / `unpin` | レコードを自動ローテーションから保護・解除します。 |
| `delete` / `wipe` | 1件を削除する、または履歴を全消去します。 |
| `status` / `pause` / `resume` | デーモンの監視状態を確認・制御します。 |
| `stats` | エントリ数、ディスク占有、デーモンのシェッド回数を表示します。 |
| `doctor` | 環境・コンポジタ・DB・ソケット・クォータを診断します（FAIL時のみ非0で終了）。 |

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

# 保存内容と環境の健全性を確認する
y4p stats
y4p doctor
```

### ユーザーサービスとして常駐させる

`systemd` のユーザーユニットが [`contrib/systemd/y4p.service`](contrib/systemd/y4p.service) にあります。`Restart=on-failure` が機能するのは、`y4p daemon` が起動失敗・コンポジタ切断を非0で、要求された停止だけを `0` で終えるためです。

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/y4p.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now y4p.service
```

以降は `y4p status` と `y4p doctor` がこのインスタンスを参照し、ログは `journalctl --user -u y4p` で確認できます。

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

# 実コンポジタでのマトリクス試験（cage/sway + wl-clipboard + ImageMagick）。
# ヘッドレスコンポジタが無い環境では [SKIP] を出して exit 0
bash scripts/e2e_compositor.sh
```

</details>

---

## 設定

設定なしでも、妥当な既定値でそのまま動作します。

履歴の保持件数、MIMEフィルタ、画像サニタイズの有効化などを変更する場合は、`$XDG_CONFIG_HOME/y4p/y4p.toml`（未設定なら `~/.config/y4p/y4p.toml`）に設定ファイルを置いてください。設定ファイルが無い、または壊れていても、デーモンは既定値で起動します。

解説付きのひな形が [`y4p.toml.example`](y4p.toml.example) にあります。

```bash
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/y4p"
cp y4p.toml.example "${XDG_CONFIG_HOME:-$HOME/.config}/y4p/y4p.toml"
```

---

## アーキテクチャ概要

`y4p` はCargoワークスペースです。ルートのパッケージは薄い組み立て役（`src/main.rs`）に徹し、各ドメインは専用のクレートに分かれています。依存関係は常に `y4p-core` に向かって一方向に流れ、循環はコンパイラが拒否します。

- **[`crates/core`](crates/core)（`y4p-core`）**: 定数、XDGパス、`y4p.toml` パーサ、MIMEユーティリティ、`Payload` の語彙、パイプ容量とカーネルコピーのプリミティブ、終了フラグ。
- **[`crates/storage`](crates/storage)（`y4p-storage`）**: SQLite v4ファサード、スキーマのマイグレーション、インラインのペイロード保存、レガシーチャンク行の互換読み取り、LRUクォータ付きのファイルシステムキャッシュ。
- **[`crates/image`](crates/image)（`y4p-image`）**: 画像の判別、オプトインのサニタイズ、サンドボックス化された変換、サーキットブレーカー、同時実行制限。
- **[`crates/wayland`](crates/wayland)（`y4p-wayland`）**: `ext-data-control-v1` のハンドラ、上限付きのクリップボード読み取り、取り込みキュー、イーグレス。
- **[`crates/daemon`](crates/daemon)（`y4p-daemon`）**: イベントループ、IPCソケット、バックグラウンドワーカー（任意の画像ルーティング・ハッシュ計算・永続化）。
- **[`crates/cli`](crates/cli)（`y4p-cli`）**: 厳格な引数解析、コマンドハンドラ、出力整形。

---

## ドキュメント

このREADMEは、あえて短くまとめた入門ガイドです。

デーモンがなぜ単一スレッドでI/Oを多重化するのか、Pin保護が履歴ローテーションとどう連携するのか、不変IDがなぜ必要なのか——そうした内部設計の「なぜ」は、`docs/` に技術文書としてまとめています（英語）。入口は [`docs/00_overview.md`](docs/00_overview.md) です。ワークスペースの構成、クレートの責務、依存の向き、取り込みのストリーム契約を扱い、`docs/` 全体への案内も兼ねています。

プロジェクトの運営・計画・参照文書:

- [`.github/POLICY.md`](.github/POLICY.md): コントリビューション方針および品質基準（英語）。
- [`docs/history/PERFORMANCE.md`](docs/history/PERFORMANCE.md): 性能の定量ベースライン（レイテンシ、アロケーション、メモリ上限）と歴史的な目標（英語）。再現は `cargo bench -p y4p-daemon --bench baseline`。
- [`ROADMAP_v0.10.md`](ROADMAP_v0.10.md): v0.10.0 (Subtraction) へ向かう v0.9.x の削減マイルストーン（英語）。
- [`README.md`](README.md): 英語版README。

---

## ライセンス

GPL-3.0-or-later

Copyright (c) 2026 yosana (y0sh-dev)
