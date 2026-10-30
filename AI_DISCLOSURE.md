# AI Usage Disclosure

This document describes how AI-assisted tools were used during the development and testing of Skot. The goal is to provide transparency about their contributions and the extent of human involvement.

## 1. Development

The majority of Skot's core implementation and its verification were performed by a human.

AI assistance was used for some routine implementation tasks, including:

- Generating and refining CLI argument descriptions using `clap`.
- Generating Rust unit tests.
- Implementing minor bug fixes identified during usability testing.

AI-generated tests and bug fixes were reviewed and verified by a human. AI assistance was primarily used to reduce the effort involved in routine tasks rather than delegate the core engineering and architectural decisions.

Neovim Supermaven autocomplete was also used during development. As with other code-completion tools, its contributions were interspersed with ordinary development rather than constituting a separate implementation phase.

## 2. Automated UX Testing

AI agents were used to evaluate Skot's usability and general user experience, with particular attention to how readily an unfamiliar user could discover and understand its workflows.

The evaluation involved Claude Opus 4.6, DeepSeek, and GPT-6 Luna.

The agents were given intentionally vague instructions. They were expected to explore the CLI with minimal guidance, attempt meaningful workflows, and document anything they could not understand or complete. This was deliberate: the objective was to identify usability problems that might be overlooked by the developer, who was already familiar with the tool.

Each agent was instructed to maintain a reproducible log of its work, recording significant commands, observations, errors, and usability friction. The resulting reports were then analyzed by Claude, which compiled the findings into an actionable report.

Some minor bugs identified during these evaluations were subsequently fixed with AI assistance and verified by a human.

These evaluations were intended to complement conventional automated tests, not replace them. Agent feedback was treated as input for identifying potential problems, rather than as proof that a particular usability issue existed or that a workflow was universally usable.

## 3. Original UX Evaluation Prompt

The following is the prompt used for the autonomous UX evaluation, reproduced verbatim:

> Hello, I have a CLI tool named 'skot' that you can execute right away. Your task is to autonomously assess its usability and general UX.
>
> Workspace & Test Organization:
> The current working directory is empty, but do not dump everything into the root folder. Please organize your tests into clean, isolated subfolders (e.g., ./test-a/, ./test-pr-patch/) for different scenarios or test cases. Avoid wandering off into /tmp, keep your workspace contained within these structured subfolders to make reproduction clean and straightforward.
>
> Documentation & Reproducibility:
> As you explore and test the tool, you must maintain a running log of your session in a file named skot-eval-report.md. For complete reproducibility, every time you run a significant command or encounter a roadblock, record:
>
> Test Directory: Which subfolder/scenario you are working in.
>
> Command Run: The exact CLI command you executed.
>
> Observation: What happened (expected vs. actual output).
>
> Friction / UX Bug: Any rough edges, confusing UX, or errors encountered. If a feature is unintuitive or you can't figure that out, log it explicitly as a UX bug.
>
> Project Scope:
> On choosing a project, pick a lightweight one that utilizes tools already installed on this machine (like clang, make, or Rust toolchains). Prioritize a quick, meaningful usability analysis over deep technical exhaustion, keeping in mind this tool is built for humans and agents alike. If you encounter something off then record and continue, dont go on side quests trying to figure out how it internally works, the goal is to flag such cases not fix them. Good luck!

## 4. Verification and Responsibility

Skot has extensive unit and end-to-end test coverage. AI-assisted testing and development were additional tools in the development process; they did not replace the project's conventional tests or human verification.

The project maintainer remains responsible for the codebase, technical decisions, releases, and evaluation of proposed changes.

AI-assisted usability testing provides useful observations, but it has limitations. Agents may misunderstand instructions, overlook problems, or encounter difficulties that differ from those of human users. Conversely, an agent successfully completing a workflow does not establish that the workflow is intuitive to everyone.

Independent feedback from human users remains important for validating the usability of Skot in real-world workflows.

---

This disclosure describes AI usage to the best of the maintainer's recollection.
It is intended to document the nature and scope of that assistance, not to claim that AI-assisted work is inherently more or less reliable than work performed by other means.
