# Owner decisions

Decisions the owner made. They override defaults in `DESIGN.md` section 12 and in the specs.

## 2026-11-25

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Licence | The product must carry no code, structure, names or prose of the earlier upstream project. Ideas only. A commercial sale is possible later. |
| 2 | Name | The product is Cataly. No earlier product name appears anywhere. |
| 3 | Relays | Keep Discord, Telegram, Slack and voice. Keep the terminal runtime (tern). |
| 4 | Windows | No Windows build of the new Cataly is planned. Telegram intake on Windows is dropped. The earlier product keeps its Windows CI as it is. |
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
| 15 | Fastest wake without weaker safety | An owner request to the lead is written to OMP at once; the durable commit of its arrival in the lead's inbox, its delivery record and the relay's hand-off record runs alongside instead of before (commits stay `synchronous=FULL`). A message is never delivered twice and never lost: the early wake carries a delivery marker derived from the request ID, and after a crash any message not read live in that hub run is looked up in the lead's transcript before any send (restart, catch-up, a hand-off made again). |
| 16 | Home backup | Every Cataly user's home backup goes to a private GitHub repository named `cataly-home` (under the authenticated `gh` user) by default. The home is the state folder; the config file is copied in and the store goes in as an online-backup snapshot, never the live SQLite files. Secrets and token files stay out; the secret scan runs before every push. A public repository is refused and never created. |
| 17 | Release contents | Releases ship with voice built in (`--features voice`) on every target. One release build, not a voice and a no-voice variant. |
| 18 | Earlier product | The earlier product is finished. Nothing is migrated from it: no bot, channels, settings or state. A new Discord application, bot and channels are created for Cataly. |

## 2026-11-26

| # | Topic | Decision |
| --- | --- | --- |
| 19 | Approvals (DESIGN 12.2 A13; replaces the coordinator's pending P1) | Approvals follow the owner's own OMP settings. Cataly forces no approval mode and adds no read-only command list: sessions start without `--approval-mode`, and the overlay carries no `bash.patterns`, so OMP uses the owner's global configuration or its own default. Kept: Cataly's own tools are registered with `policy: allow` and `loadMode: "essential"`; the hub's shell veto (rule 0 for `cataly` CLI mutations and spend commands, and the "call your own tool directly" refusal); `eval` denied; every dialog OMP does open goes to the owner as a Discord card in the owner's language with owner-only buttons. |
| 20 | Receipt store (HOST req. 57-60; set by the coordinator) | Receipts are kept per session: a receipt belongs to the session its message was written into, and the cap of 512 counts per session. A full store never refuses a message or holds a room: when a session passes 512, its oldest settled receipts (delivered, or their inbox row gone) are evicted first. Pending, sent, in-doubt and failed receipts, and any whose relay hand-off is not settled yet, are never evicted. A repeat of a request ID is caught while its receipt is kept: all open ones, the newest 512 per session, at most 7 days (retention). Duplicate lookup spans the target's sessions, so a retry after a lead restart is still caught. This replaces HOST req. 59's "when full, new ids are refused". |
