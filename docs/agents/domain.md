# Domain docs

Use a single-context layout:

- `CONTEXT.md` at the repository root: shared domain vocabulary.
- `docs/adr/`: architectural decisions.

Before exploring code, read CONTEXT.md and ADRs relevant to the task.
If absent, proceed silently. Create them through domain-modeling
when terms or decisions are resolved; avoid empty placeholders.

Use the vocabulary defined in CONTEXT.md in code, issues, and reports.
Identify terminology gaps for domain-modeling.

When a proposal conflicts with an ADR, cite the ADR and explain why
the decision should be reconsidered.

For steward planning, also read `docs/waku-project-handoff.md`.
The historical design is input to specification work and requires
the current-source checks recorded there.
