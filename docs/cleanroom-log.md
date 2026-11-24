# Clean-room record

| Date | Item | Role | Allowed to read | Result |
| --- | --- | --- | --- | --- |
| 2026-11-25 | Rules, principles, repository created | Coordinator (has seen the earlier tree; writes no code or design here) | everything | `docs/cleanroom.md`, `docs/principles.md` |
| 2026-11-25 | 13 behaviour specs written | Analysis agents (one per area) | the earlier tree, read-only snapshot | drafts outside this repository |
| 2026-11-25 | Spec audit | Coordinator | drafts and the earlier tree | Identifier scan against 5,862 names and 381 file names from the earlier code: only third-party public names (OMP, Discord, Telegram, Slack) matched. Seven-word overlap with all earlier text: 0 to 77 words per spec (under 1.3%), short factual fragments; eight sentences reworded. No code blocks, no secrets or IDs. |
| 2026-11-25 | Specs committed under `spec/` | Coordinator | audited drafts | 13 files |
