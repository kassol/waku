# Issue tracker: GitHub

Issues and specs live in `kassol/waku`. Use the `gh` CLI.
Always pass `--repo kassol/waku`; API paths must target `repos/kassol/waku`.
Never create or modify issues, PRs, labels, or comments in `egoist/waku`.

## Operations

- Create: `gh issue create --repo kassol/waku --title "..." --body-file <file>`.
- Read: `gh issue view <number> --repo kassol/waku --comments`.
- List: `gh issue list --repo kassol/waku --state open --json number,title,body,labels,assignees`.
- Comment: `gh issue comment <number> --repo kassol/waku --body-file <file>`.
- Labels: `gh issue edit <number> --repo kassol/waku --add-label "..."` or `--remove-label "..."`.
- Close: `gh issue close <number> --repo kassol/waku`.

Use UTF-8 files with actual newlines for multiline content.
Read existing comments and labels before changing an issue.

## Skill conventions

“Publish to the issue tracker” means create a GitHub issue.
“Fetch the relevant ticket” means read the issue and its comments.

PRs as a request surface: no.

GitHub issues and PRs share numbers. Resolve the object type before acting.

## Wayfinding

- Store the map in one issue labelled `wayfinder:map`.
- Link tickets as native sub-issues. If unavailable, use a task list
  in the map and `Part of #<map>` in each ticket.
- Use `wayfinder:research`, `wayfinder:prototype`, `wayfinder:grilling`,
  or `wayfinder:task` for ticket type.
- Store blockers as native issue dependencies, using issue database IDs.
  If unavailable, use `Blocked by: #<number>` in the ticket body.
- Select the first open, unassigned ticket in map order whose blockers
  are all closed.
- Claim by assigning the ticket to the executing developer.
- Resolve with findings and validation evidence, close the ticket,
  and add a linked decision summary to the map.
