# Milestone 6: terminal runtime (build notes)

Scope: HOST-30, HOST-31, EXT-20, EXT-21, EXT-25, EXT-26, EXT-30, COORD-14, PATCH-01 rules 6, 7, 12, 13, 14.
`import/` held no tern runtime; everything was written from the specs and `DESIGN.md` 4.3.

## Sources checked

- **Tern:** the installed Tern 0.9.33 (`tern --help`, `tern new --help`): `tern new tab [SESSION] [--cwd DIR]
  [--keep-open] -- COMMAND...`, `--json` prints what a command did, `tern close BLOCK`. The hub uses exactly
  these; `tests/support/fake-tern` stands in for them.
- **OMP:** the extension API in OMP's public `docs/extensions.md` (main branch, read 2026-11-25) and stock OMP
  19.1.1 run as a third-party product in `tests/contract_tern.rs`: `pi.sendMessage` (`triggerTurn`,
  `deliverAs: "followUp"`), `pi.sendUserMessage(.., { deliverAs: "steer" })`, `ctx.abort`, `ctx.shutdown`,
  `ctx.isIdle`, `pi.setModel` with `ctx.modelRegistry.find`, `ctx.sessionModes.getState/set` (TUI only),
  `session_before_switch|branch|tree` returning `{ cancel: true }`, `session_clear`, `tool_call` block
  results, optional `ctx.ui.askDialog`. The custom message is persisted as a `custom_message` entry; the
  contract case confirms a wake from that entry in stock OMP's session file.

## Choices

- **One runtime enum.** `host::hosted::{Hosted, Hosting}` implement `Runtime`/`Control` over the two runtimes;
  the lead and lanes are `Lead<Hosted>`/`Lanes<Hosted>`. The factory picks the runtime per start from
  `[sessions] runtime` (`headless` default, `tern`), `tern` (executable, default `tern` on PATH) and
  `tern_session` (the Tern session new tabs open in). The lead's session row records the runtime used.
- **The port.** `host::tern::Port` is the hub side of one terminal session, registered in `host::bridge::Links`
  under the session handle. `hub::bridge` hands it the hello (attach), every line (keep-alive), events,
  `bridge.reply` answers to numbered commands and `bridge.form` offers. Commands travel as
  `{"item":{"cmd":{n, op, session, ..}}}` on the existing connection; no new socket and no new framing.
- **Identity.** The first hello (single-use token, 60 s life for a tern block, `Links::issue_for`) binds the
  session ID, file and OMP pid. A reconnect must carry the same pid; the same pid with another session ends
  the old identity (stop record `exited`). A nested OMP inheriting the used token is told `none`.
- **Status.** Built with the shared `dialogs::status` from bridge facts (idle, model, thinking, goal); no link
  means unknown and blocked input. Approvals and plan reviews are `unsupported` dialogs: the session is
  blocked and `answer` refuses them ("belongs to the person at the terminal").
- **Delivery.** The batch's `[cataly:<id>]` marker is the delivery ID. Wake: `sendMessage` custom
  `cataly-delivery` with `details.delivery`; steer: `sendUserMessage` steer, refused when idle. Over 64 KiB the
  text goes to `<state>/files/tern/delivery-<id>.txt` and the agent gets a pointer. "not current" maps to
  `NotReady`, so the inbox keeps the rows pending (store `superseded` -> `pending`). `transcript::find_marker`
  now also accepts `custom_message` entries (and `custom` role messages).
- **Misdelivery (PATCH-01 rule 9).** `Mailbox::check` searches the current session's file when it differs
  from the record's; found there: `misdelivered`, never resent, one `delivery.failed` event with reason
  `misdelivered: ...` (owner alert) and one log line with both session IDs.
- **Session left (rule 13).** While the same process runs another session, a sent record waits up to 24 h for
  its session to be current again, then fails; headless sessions are unaffected (their session never moves).
- **Switch guard (EXT-26, rules 6-7).** In the bridge: a user's switch/branch/tree is cancelled with a notice
  while a hub command is applied; otherwise the session is marked changing until the after-event or 10 s,
  when the current session is reported (`session_current`). Commands for another session or during a change
  are refused "not current" without calling OMP.
- **Forms (EXT-30).** For the `ask` tool in a tern session with `ctx.ui.askDialog`, the bridge offers the
  questions (`bridge.form`); the hub turns them into an `ask` dialog the relays post. The bridge opens the
  local dialog itself and races it with the remote answer; the loser is withdrawn (abort signal, or the form
  closed on the hub). The tool call returns a block result whose reason carries the answer.
- **Lost lanes.** No line for 45 s after a disconnect emits `LinkLost` (lane `lost`, `lane.lost` event);
  the same process back with the same session emits `LinkRestored` (lane un-lost, inbox flushed).
- **COORD-14.** Nothing is typed on a screen, so there is no typed-but-not-submitted state.

## Deviations

- **Bridge size.** Kept at 599 lines by compacting existing code (a forwarding table for plain events,
  shorter helpers, a few paired field declarations); no behaviour removed.
- **Rule 12 and 14 (reload, clear).** OMP's `session_clear` keeps the session ID, file and persisted history
  (extensions.md), so records sent before a clear are still found in the file and count as delivered; the
  clear is reported as a notice. A reload with the same session ID and file changes nothing the hub tracks:
  sent records keep their 30 s confirmation window. Neither needs extra state.
- **A stored session stays on its runtime (HOST req. 91)** is not enforced for resumes: the runtime is taken
  from the config at each start. The lead row records which runtime ran.
- **`ctx.ui.askDialog` signature** (questions object, `{ signal }`) is taken from the docs' method list and
  verified only against the bridge test's fake; stock OMP's TUI form path is not exercised by a contract case.
- **Spec 30 "set model refused while a turn is running"** holds; `goal` takes only pause and resume from the hub
  (TUI session modes), as DESIGN 4.3 says.
- One `sed -i` was used on `src/hub/bridge.rs` for a one-line type path fix, against the edit-tool rule.

## Tests

- `src/host/tern/tests.rs`: missing executable refused with the hint (check 6), block command line, deliveries
  and "not current" (check 2), interrupt/model/goal/shutdown, approvals never answered and mirrored forms
  answered (check 4), lost and restored link, identity rules (check 5), user switch.
- `extension/cataly.test.ts`: commands, de-duplicated delivery IDs, switch guard, first-answer-wins form.
- `tests/tern.rs` (hub + Tern stand-in in a pty + fake OMP TUI mode + a test bridge client): checks 1, 2, 3, 6.
- `tests/contract_tern.rs` (stock OMP TUI in the stand-in's pty, loopback model): check 1 and the wake (check
  2) end to end, and the measurements.
