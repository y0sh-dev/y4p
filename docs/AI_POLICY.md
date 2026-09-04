
## Generative AI (LLM) Usage Policy and Guidelines

This project embraces the use of Generative AI (LLMs) for complex problem-solving, refactoring, and code generation. However, to maintain the integrity and quality of the codebase, contributors must adhere to the following principles.

### Core Philosophy
* **Technical Understanding:** You must personally comprehend why an AI-proposed fix is necessary and verify its correctness. Do not blindly merge code you cannot maintain.
* **Reviewing the Current Repository:** Ensure the issue hasn't already been resolved or discussed before applying AI-generated solutions.
* **Careful Consideration:** Always review AI suggestions within the context of the existing architecture and its impact on other components.

### Declaration of AI Usage (Pull Requests Only)
To keep branch names and commit history clean and focused on *intent* (`feat/`, `fix/`, `docs/`), **do not include AI usage declarations in branch names or commit messages.**

Instead, AI usage must be declared **exclusively in the Pull Request description**.
Keep it minimal and strictly to the point:
* **Model:** (e.g., Claude 3.5 Sonnet, Gemini)
* **Purpose:** (e.g., Refactoring the daemon event loop, Generating boilerplate)

*Note: PRs with ambiguous intent or insufficient human verification of AI-generated code may be closed.*
