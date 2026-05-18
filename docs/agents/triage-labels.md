# Triage labels

Canonical triage roles map 1:1 to GitHub labels in this repo. All five exist; do not invent synonyms.

| Canonical role     | Repo label         | Meaning                                          |
| ------------------ | ------------------ | ------------------------------------------------ |
| `needs-triage`     | `needs-triage`     | Maintainer needs to evaluate this issue          |
| `needs-info`       | `needs-info`       | Waiting on reporter for more information         |
| `ready-for-agent`  | `ready-for-agent`  | Fully specified, AFK-ready for an autonomous agent |
| `ready-for-human`  | `ready-for-human`  | Requires human implementation                    |
| `wontfix`          | `wontfix`          | Will not be actioned                             |

Pre-existing repo labels (`bug`, `enhancement`, `documentation`, `python:uv`, `rust`, `dependencies`, ...) are domain/scope labels and may co-exist with the triage label. Exactly one triage label per open issue.
