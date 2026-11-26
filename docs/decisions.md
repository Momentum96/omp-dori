# Product decisions

Decisions the owner made that shape the product. They override defaults in `DESIGN.md` section 12
and in the specs. Numbers are stable because code and `DESIGN.md` cite them; numbers not listed
concern the project's origin, which `docs/provenance.md` covers.

| # | Topic | Decision |
| --- | --- | --- |
| 1 | Licence | AGPL-3.0-only with a commercial licence offered by vntrevx. Cataly carries only code its copyright holder holds the rights to (`docs/provenance.md`). |
| 2 | Name | The product, its binary, bridge, socket and paths are all named `cataly`. |
| 3 | Relays | Discord, Telegram, Slack and voice, and the terminal runtime (tern). |
| 4 | Windows | No Windows build is planned; Telegram intake does not run on Windows. |
| 5 | Lane goals | Lanes use OMP goals. |
| 6 | Helper session | No always-on second model session. The lead is the only standing session. |
| 7 | Model fallback | On. When a model's quota runs out, the session may fall back to another configured model. This never redeems a reset, buys credits or enables paid overage; those stay refused in code. |
| 8 | Stuck OMP exit | The hub may terminate, then kill, an OMP child that does not exit after its input is closed. |
| 9 | Hub restart | Running lanes are brought back from their session files. |
| 10 | Slack senders | Restricted to an allowlist. Default: the owner only. |
| 11 | Voice transport | WebRTC next to WebSocket. Before a release that changes voice, both are tested against the real realtime API by a gated live test whose cost the owner approves. |
| 12 | Updates | Notice only. `cataly update` installs. Nothing installs by itself. |
| 13 | Weight | Always light. Reuse code where it can be reused. One implementation per concern, no duplication. |
| 15 | Fastest wake without weaker safety | An owner request to the lead is written to OMP at once; the durable commit of its arrival in the lead's inbox, its delivery record and the relay's hand-off record runs alongside instead of before (commits stay `synchronous=FULL`). A message is never delivered twice and never lost: the early wake carries a delivery marker derived from the request ID, and after a crash any message not read live in that hub run is looked up in the lead's transcript before any send (restart, catch-up, a hand-off made again). |
| 16 | Home backup | Every Cataly user's home backup goes to a private GitHub repository named `cataly-home` (under the authenticated `gh` user) by default. The home is the state folder; the backup holds a copy of the config file and an online-backup snapshot of the store, never the live SQLite files. Secrets and token files stay out; the secret scan runs before every push. A public repository is refused and never created. |
| 17 | Release contents | Releases ship with voice built in (`--features voice`) on every target. One release build, not a voice and a no-voice variant. |
| 19 | Approvals (`DESIGN.md` 12.2 A13) | Approvals follow the owner's own OMP settings. Cataly forces no approval mode and adds no read-only command list: sessions start without `--approval-mode`, and the overlay carries no `bash.patterns`, so OMP uses the owner's global configuration or its own default. Kept: Cataly's own tools are registered with `policy: allow` and `loadMode: "essential"`; the hub's shell veto (rule 0 for `cataly` CLI mutations and spend commands, and the "call your own tool directly" refusal); `eval` denied; every dialog OMP does open goes to the owner as a Discord card in the owner's language with owner-only buttons. |
| 20 | Receipt store (HOST req. 57-60) | Receipts are kept per session: a receipt belongs to the session its message was written into, and the cap of 512 counts per session. A full store never refuses a message or holds a room: when a session passes 512, its oldest settled receipts (delivered, or their inbox row gone) are evicted first. Pending, sent, in-doubt and failed receipts, and any whose relay hand-off is not settled yet, are never evicted. A repeat of a request ID is caught while its receipt is kept: all open ones, the newest 512 per session, at most 7 days (retention). Duplicate lookup spans the target's sessions, so a retry after a lead restart is still caught. This replaces HOST req. 59's "when full, new ids are refused". |
