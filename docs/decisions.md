# Owner decisions

Decisions the owner made. They override defaults in `DESIGN.md` section 12 and in the specs.

## 2026-11-25

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Licence | The product must carry no code, structure, names or prose of the earlier upstream project. Ideas only. A commercial sale is possible later. |
| 2 | Name | The product is Cataly. No earlier product name appears anywhere. |
| 3 | Relays | Keep Discord, Telegram, Slack and voice. Keep the terminal runtime (tern). |
| 4 | Windows | No Windows build of cataly-core is planned. Telegram intake on Windows is dropped. The earlier product keeps its Windows CI as it is. |
| 5 | Lane goals | Lanes use OMP goals. |
| 6 | Helper session | No always-on second model session. The lead is the only standing session. |
| 7 | Model fallback | On. When a model's quota runs out, the session may fall back to another configured model. This never redeems a reset, buys credits or enables paid overage; those stay refused in code. |
| 8 | Stuck OMP exit | The hub may terminate, then kill, an OMP child that does not exit after its input is closed. |
| 9 | Hub restart | Running lanes are brought back from their session files. |
| 10 | Slack senders | Restricted to an allowlist. Default: the owner only. |
| 11 | Voice transport | Keep WebRTC next to WebSocket. It is tested against the real realtime API with the owner's API key before release; the owner approves the cost of that test. |
| 12 | Updates | Notice only. `cataly update` installs. Nothing installs by itself. |
| 13 | Weight | Always light. Reuse code where it can be reused. One implementation per concern, no duplication. |
| 14 | Earlier Rust | Rust that the owner wrote himself in the earlier tree may be reused after a per-file check (see `docs/cleanroom.md`, "Vetted imports"). |
| 16 | Home backup | Every Cataly user's home backup goes to a private GitHub repository named `cataly-home` (under the authenticated `gh` user) by default. The home is the state folder; the config file is copied in and the store goes in as an online-backup snapshot, never the live SQLite files. Secrets and token files stay out; the secret scan runs before every push. A public repository is refused and never created. |
