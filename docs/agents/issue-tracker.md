# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues in `SystemicVoid/parakeet-stt`.
Use the `gh` CLI for all operations; it infers the repo from `git remote`.

## Conventions

- **Create**: `gh issue create --title "..." --body "..."` (heredoc for multi-line bodies).
- **Read**: `gh issue view <number> --comments`.
- **List**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'`.
- **Comment**: `gh issue comment <number> --body "..."`.
- **Labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`.
- **Close**: `gh issue close <number> --comment "..."`.

## Mapping skill verbs

- *"publish to the issue tracker"* → `gh issue create`
- *"fetch the relevant ticket"* → `gh issue view <number> --comments`
- *"apply the X triage label"* → `gh issue edit <number> --add-label X` (see [triage-labels.md](triage-labels.md))
