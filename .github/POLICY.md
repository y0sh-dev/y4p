# Contribution Policy

This project treats Generative AI (LLMs), static analysers, formatters, and code generators as ordinary engineering tools. They are neither given special status nor restricted; what matters is the quality, clarity, and maintainability of the resulting patch.

Model names and AI usage declarations are not required in branch names, commit messages, or pull request descriptions.

---

## Core Principles

Every contribution, regardless of how it was authored, must adhere to the following standards:

1. **Comprehension and ownership.** You must personally understand every line you submit, why it is necessary, and how it fits the existing architecture (see [`docs/00_overview.md`](../docs/00_overview.md)). Never submit code you cannot explain or maintain.
2. **Empirical verification.** Run the workspace verification suite (`bash scripts/check.sh`) and verify relevant edge cases before submitting. Do not rely on assumptions in place of observed test output.
3. **Intent-focused history.** Keep branch names, commit messages, and pull requests focused strictly on technical intent (`feat/`, `fix/`, `docs/`), free of tool-attribution noise.

---

## Review Standard

Pull requests with unclear intent, unverified assumptions, or changes the author cannot substantiate may be closed regardless of the tools used to produce them.
