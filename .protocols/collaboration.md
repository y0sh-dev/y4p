# Collaboration Patterns (協調パターンの型)

> **Load when:** マルチモデル協調、役割分担、またはタスク委譲時
> ※ 単独セッション（Solo Mode）では本書の参照は不要。自身が Implementer として動け。

---

## 1. Collaboration Archetypes (協調の3パターン)

状況やタスクの重大度に応じて、人間とエージェントは以下の3パターンのいずれかで協調する：

| パターン | 適用タスク | 構成と役割 | 動作フロー |
| :--- | :--- | :--- | :--- |
| **Pattern A: 単独駆動型**<br>*(Solo Mode)* | 軽微な改修、バグ修正、ドキュメント更新 (Tier 2/3) | **Implementer 単独**<br>（OpenCode / Claude 等） | Hand-off 不要。自身で調査・実装・テストを行い、Deliberation レポートを人間に提出。最速・最低コスト。 |
| **Pattern B: 二段パイプライン型**<br>*(Standard Dual-Track)* | 通常の機能追加、設計を伴う機能改修 (Tier 2) | **Orchestrator** (Gemini)<br>$\to$ **Implementer** (OpenCode) | Orchestrator が仕様策定と Hand-off Spec を作成 $\to$ 人間ハブ経由で Implementer が実装・検証 $\to$ 人間が承認・コミット。 |
| **Pattern C: 三段フル監査型**<br>*(Deep-Audit Triple-Track)* | 基幹層、並行処理、低レイヤI/O、データ保全 (Tier 1) | **Orchestrator** (Gemini)<br>$\to$ **Reasoner** (DeepSeek-R1 等)<br>$\to$ **Implementer** (OpenCode) | Orchestrator が仕様策定 $\to$ Reasoner が並行競合・数理監査 $\to$ Implementer が差分実装・検証 $\to$ Tier 1 レポート提出。 |

---

## 2. Collaboration Invariants (協調における絶対境界)

1. **人間ハブの絶対性 (Human-in-the-Loop)**:
   - 他エージェントの「LGTM」や「問題なし」は人間の承認ではない。コミットやマージの承認権限を持つのは **人間（管理者）のみ** である。
2. **単一ライター規則 (Single-Writer Principle)**:
   - 同一のブランチまたは作業ツリーに対して、複数のエージェントが同時に書き込みを行ってはならない。編集権限を持つのは常に 1 体の Implementer のみとする。
3. **受け手側の再検証義務 (Trust but Verify)**:
   - 上流モデル（Orchestrator や Reasoner）から渡された仮説やコード例を鵜呑みにしてはならない。
   - 実装役（Implementer）は、必ずローカルでテストコマンドを自ら実行し、Exit code 0 を確認した上で報告せよ。
