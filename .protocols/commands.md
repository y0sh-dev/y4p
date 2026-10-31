# Project Commands — y4p-rs

> **Load when:** ビルド、テスト、リンター、サニタイザ、またはベンチマーク実行時

---

## 1. Primary Verification (第一検証: 常時使用)

すべての変更後に実行し、Exit code 0 を確認すべき標準チェッカー：

```bash
# Workspace 全体のテスト (569 tests) + Strict Clippy (-D warnings)
bash scripts/check.sh
```

- **所要時間**: 約 15〜30 秒（キャッシュ状況による）
- **合格基準**: 全テスト Pass、Clippy 警告 0 件

---

## 2. Granular Development Commands (個別開発・高速ループ)

```bash
# 1. 高速型チェック・コンパイル確認
cargo check --workspace --all-targets

# 2. 特定クレートのみの単体テスト
cargo test -p y4p-core
cargo test -p y4p-wayland
cargo test -p y4p-storage
cargo test -p y4p-image
cargo test -p y4p-daemon

# 3. 特定テスト関数の単体実行
cargo test -p y4p-core -- test_name

# 4. Strict Linter の単体実行
RUSTFLAGS="-D warnings" cargo clippy --workspace --all-targets -- -D warnings
```

---

## 3. Deep Verification & Sanitizers (深層検証・CI 相当)

Tier 1 変更時やリリース前検証で使用する高負荷コマンド：

```bash
# Miri (Undefined Behaviour 検証: core utils 対象)
cargo miri test -p y4p-core

# AddressSanitizer (メモリ破壊・リーク検証)
RUSTFLAGS="-Zsanitizer=address" cargo test -p y4p-core --target x86_64-unknown-linux-gnu

# ベンチマーク・スモークテスト
cargo bench --bench micro_benchmarks -- --test
```
