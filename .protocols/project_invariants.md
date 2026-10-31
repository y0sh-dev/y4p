# Project Invariants — y4p-rs

> **Load when:** プロジェクトの基幹方針、クレート境界、または例外規定の確認時
> ※ 全体安全核（言語分離、人間最優先、5回リミッター、保護ブランチ等）は `~/.agent-protocols/CARD.md` を参照。

---

## 1. Architectural Invariants (アーキテクチャの不変条件)

1. **`crates/storage/` の不変性（原則維持）**:
   - 原則として `crates/storage/` 以下のファイルは変更しない（0 lines changed）。
   - ストレージ層の改修が必要な場合は、スキーマ互換性・マイグレーション・データ保全性の完全な論理証明を必須とする。
2. **外部クレート追加の厳禁（Zero External Dependency）**:
   - 原則として `Cargo.toml` に新しい外部クレートを追加せず、標準ライブラリ（`std`）のスライス走査・パーサのみで完結させる。
   - 導入が必要な場合は、バイナリサイズ、ビルド時間、攻撃面、自前実装との費用対効果の論理的比較を提示し人間に承認を得よ。
3. **Strict Panic-Free Guarantee**:
   - `unwrap()`, `expect()`, `panic!()` の新規追加は厳禁。
   - `RUSTFLAGS="-D warnings" cargo clippy --all-targets -- -D warnings` を警告0でパスすること。

---

## 2. Deliverables & Documentation Invariants

1. **言語基準 (British English)**:
   - コード内コメント、Git コミットメッセージ、Pull Request、ドキュメントは、原則として **British English (UK: e.g. sanitise, initialise, behaviour, prioritise)** で統一する。
2. **二段階ドキュメンテーション**:
   - 実装中はコード内に詳細な Why コメントを記述し、リリース直前（`.N.9` またはリリース直前）に `docs/` 配下へ体系的に蒸留・移植する。
