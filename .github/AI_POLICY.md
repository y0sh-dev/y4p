# Generative AI (LLM) Usage Policy and Guidelines

This project embraces the use of Generative AI (LLMs) for complex problem-solving, refactoring and code generation. To maintain the integrity and quality of the codebase, contributors must adhere to the following principles.

---

## 1. Core Philosophy

1. **Technical understanding.** You must personally comprehend why an AI-proposed change is necessary and verify its correctness. Do not blindly merge code you cannot maintain.
2. **Review the current repository.** Ensure the issue has not already been resolved or discussed before applying an AI-generated solution.
3. **Careful consideration.** Review every AI suggestion in the context of the existing architecture (see [`docs/00_overview.md`](../docs/00_overview.md)) and its impact on other components.

---

## 2. Declaration of AI Usage (Pull Requests Only)

To keep branch names and commit history focused on *intent* (`feat/`, `fix/`, `docs/`), **do not include AI usage declarations in branch names or commit messages.**

Declare AI usage **exclusively in the Pull Request description**, using the "AI Usage" section of the [pull request template](PULL_REQUEST_TEMPLATE.md). Keep it minimal and strictly to the point:

| Field | Content | Example |
| :--- | :--- | :--- |
| **Model** | The model(s) used | Claude 3.5 Sonnet, Gemini |
| **Purpose** | What the model was used for | Refactoring the daemon event loop; generating boilerplate |

---

## 3. Consequences

PRs with ambiguous intent, or with insufficient human verification of AI-generated code, may be closed.
