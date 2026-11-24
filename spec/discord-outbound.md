# Discord outbound: behaviour spec

## Purpose

Everything the product does toward Discord on the owner's behalf: posting and editing messages,
answering the owner's requests in place, acknowledging that a request was seen, opening and
labelling work threads, putting questions and permission requests to the owner with buttons and
getting the answer back into the asking OMP session, keeping the bot visibly online, and keeping
an honest record of what the owner is still waiting for. Users are the owner (reads and answers in
Discord), the lead OMP session, worker sessions (lanes), and the daemon itself. Reading Discord
(intake, cursors, Gateway connection lifecycle) is a separate spec; this one starts where a
decision to write to Discord has been made.

"Owner question" below means OMP's interactive form request (the ask tool), mirrored to Discord.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| DISC-OUT-01 | Post text to a channel or thread, optionally as a linked reply | sessions, daemon | KEEP | Core function. |
| DISC-OUT-02 | Edit one of the bot's own messages | sessions, questions | KEEP | Needed for question cards and status updates; small. |
| DISC-OUT-03 | Split long text into Discord-sized parts, keeping code blocks intact | all posting | SIMPLIFY | Old tree has two separate implementations (TS and Rust); keep one in Rust. |
| DISC-OUT-04 | Render Markdown pipe tables as aligned monospace blocks | posting | SIMPLIFY | Useful; make it one pass in the daemon instead of a per-call option. |
| DISC-OUT-05 | Refuse text containing a shell command-substitution marker | posting | DROP | Symptom fix for text built in a shell; a structured tool call cannot have that problem. |
| DISC-OUT-06 | Resolve a destination by ID or by unique channel name | sessions | SIMPLIFY | Keep exact ID and unique-name lookup; one code path. |
| DISC-OUT-07 | Post a session's final answer as the reply to the owner's message, exactly once | lead, lanes | KEEP | The main answer path; must be idempotent across crashes. |
| DISC-OUT-08 | Read receipt reaction on admitted owner messages; general reactions | daemon, sessions | KEEP | Owner relies on it to know a message was taken. |
| DISC-OUT-09 | Track what the owner is owed and remind the lead before it stops | lead | SIMPLIFY | Mostly structural once the daemon posts the answer itself; keep a small fallback. |
| DISC-OUT-10 | Durable record of confirmed replies, linkable to reports | lead, reports | MERGE into DISC-OUT-07 | The reply outbox already holds the confirmed IDs; no second log. |
| DISC-OUT-11 | Create threads (text, from a message, forum posts with tags); change title, tags, archive | lead | KEEP | Work is organised in threads. |
| DISC-OUT-12 | Mark a thread done or working through a title prefix | lead | KEEP | Cheap, visible progress signal. |
| DISC-OUT-13 | Crash-safe, once-only thread creation for a lane, bound to the owner's original message | lanes | SIMPLIFY | Keep write-ahead idempotency; drop separate lock files and CLI layering. |
| DISC-OUT-14 | Ledger of threads the bot started so replies in them are watched (Slack) | Slack | MERGE into the Slack spec | Discord threads are discovered natively; this is Slack-only. |
| DISC-OUT-15 | Turn an OMP form into a Discord-safe question, or refuse | questions | KEEP | Without it the owner cannot answer away from the terminal. |
| DISC-OUT-16 | Run the question exchange: post, buttons, text answers, validation, correction, close | questions | KEEP | Core remote-control feature. |
| DISC-OUT-17 | Auto-pick a single recommended choice after a quiet period | questions | KEEP | Keeps unattended work moving with an explicit, visible default. |
| DISC-OUT-18 | Decide whether and where a session's question goes to Discord | questions | SIMPLIFY | One rule set in the daemon instead of per-session files plus automatic discovery. |
| DISC-OUT-19 | Cross-process question plumbing: heartbeat discovery files, IPC relay, answer files, patching the UI dialog | questions | DROP | Exists only because several processes share the job; one daemon makes it unnecessary. |
| DISC-OUT-20 | Owner approves or denies a pending permission request with buttons | sessions | SIMPLIFY | Old version covers one hard-coded demo case; generalise to OMP confirm requests. |
| DISC-OUT-21 | Keep the bot shown as online | daemon | MERGE into the intake Gateway connection | Old tree runs a whole extra process just for presence. |
| DISC-OUT-22 | Upload a bundled avatar when the bot has none | daemon | KEEP | One request at start-up; branding. |
| DISC-OUT-23 | Request timeouts, size caps, rate-limit handling, retries | all | SIMPLIFY | Two clients with different rules today; one client, one policy. |
| DISC-OUT-24 | Distinguish rejected, confirmed and uncertain writes; never blindly resend | all | KEEP | Prevents duplicate posts; matters for safety. |
| DISC-OUT-25 | File-based intake health record for the updater (process identity, lock dir) | updater | DROP | The daemon knows its own health; expose it in-process. |
| DISC-OUT-26 | Typing indicator | none | DROP | Implemented but never called. |
| DISC-OUT-27 | Loopback-only override of API and Gateway base for tests | tests | KEEP | Needed for deterministic tests; strictly limited to loopback. |
| DISC-OUT-28 | Start-up identity checks of the bot account | daemon | MERGE into the intake spec | Shared with intake; listed here only for the avatar step. |

Totals: KEEP 12, SIMPLIFY 8, MERGE 4, DROP 4.

## Requirements

### Common rules for every write (DISC-OUT-01, 02, 23, 24, 27)

1. Every message the product creates or edits must carry an allowed-mentions setting that parses no
   mentions and does not ping the replied-to user. Nobody is ever pinged by the bot.
2. Every message the product creates must carry a nonce and ask Discord to enforce it, so a repeat
   of the same create within Discord's dedupe window returns the existing message. The nonce is
   deterministic for the logical message (see 24) where a retry is possible, random otherwise.
3. Only the daemon holds the bot token. Sessions and the OMP extension ask the daemon to write; they
   never call Discord themselves.
4. Each HTTP request has a connect timeout of 5 s and a total timeout of 10 s, follows no redirects,
   and stops reading a response body above 2 MiB (treated as a failure).
5. A response is a success only if it is 2xx and, for a create or edit, the body parses and names a
   message ID in the expected channel (and, for an edit, the same message ID).
6. Outcomes are one of three: rejected (4xx other than 429; nothing happened), confirmed (success per
   5), or uncertain (network error, timeout, 408, 5xx, or a 2xx without a usable body on a write).
   The caller always learns which.
7. Reads (GET) may be retried on 5xx and network errors: up to 4 attempts, backoff 1 s, 2 s, 4 s.
8. Writes with an uncertain outcome are retried only when they carry an enforced deterministic nonce
   (creates) or are naturally idempotent (edits, reaction add/remove, thread changes that set
   absolute values). Retries use the same backoff and stop after 4 attempts or 60 s, whichever comes
   first. Otherwise the write is reported as uncertain and is not repeated automatically.
9. On 429 the client waits the requested delay (Retry-After header, else the JSON retry_after field)
   and retries if the wait is at most 60 s and the budget in 8 allows. A longer wait is reported as a
   rate-limit failure carrying the delay. The client also honours Discord's per-route bucket headers
   so it waits before sending rather than collecting 429s, and pauses all requests on a global limit.
10. A test-only override may point the API and Gateway at another base URL. It is refused unless the
    host is localhost or a loopback IP literal, the scheme is plain http/ws, and the URL has no
    credentials, query or fragment. With an override set, the real Gateway is never contacted.
11. Errors and logs never contain the bot token, interaction tokens, or URLs carrying them; they may
    contain HTTP status and Discord's numeric error code.

### Posting and editing (DISC-OUT-01, 02, 06)

12. A post request names a destination (exact channel or thread ID, or a channel name that must
    match exactly one channel in the configured guild; a leading `#` is ignored), the text, and
    optionally a message to reply to. Zero or several name matches are refused before any write.
13. Text is trimmed; empty text is refused. Input above 256 KiB is refused.
14. A forum or media channel (Discord types 15 and 16) cannot take a plain post; the request is
    refused with a hint to create a forum post (DISC-OUT-11) or to target a thread.
15. Long text is split (DISC-OUT-03) and parts are posted strictly in order, one at a time. Only the
    first part is a reply. A reply sets the message reference with fail-if-not-exists, so a deleted
    target fails the post instead of producing an unlinked message.
16. The reply is "confirmed linked" only if Discord's response for the first part has message type 19
    (reply) and references the requested message in the same channel. A success without that proof
    is reported as posted but not linked; it does not settle an obligation (DISC-OUT-09).
17. If a later part fails, posting stops; the result reports the confirmed parts' IDs and the outcome
    of the failed part. Confirmed parts are never deleted or re-sent.
18. The result of a post lists the channel, the confirmed message IDs in order, and whether the
    reply was confirmed linked.
19. An edit replaces the whole content of one of the bot's own messages, is never split, and is
    refused before any request if the text exceeds 2000 UTF-16 code units. Edit and reply cannot be
    combined.

### Answer delivery (DISC-OUT-07, 10)

20. When an owner message is handed to a session (lead or lane) as a request, the daemon records a
    pending reply for it: source channel, source message, target session. Delivery is the daemon's
    job, not the model's.
21. The answer is the final assistant message of the turn the request started: the turn ended
    normally (stop reason "stop"), the message contains no tool call, and it has non-empty visible
    text. Text the provider marks as commentary is excluded. A turn that continues automatically
    extends the wait; the answer is the last message of the final turn.
22. If, before that turn ends, another user input or request enters the same transcript, the pending
    reply is abandoned (the answer would belong to mixed work) and the obligation stays open.
23. The daemon reads the answer from OMP's public surfaces (rpc-ui events or the session file) and
    confirms it is persisted in the transcript before posting. It never posts text it only saw in a
    transient event.
24. The answer is table-rendered (DISC-OUT-04), then split (DISC-OUT-03), and each part is stored in
    a durable outbox before the first post. Each part's nonce is derived from bot ID, channel, source
    message ID and part index, so a crash-and-retry cannot duplicate a part.
25. Parts are posted in order; the first is a reply to the source message (rule 15). Each confirmed
    part is acknowledged in the outbox with its remote ID before the next is posted.
26. If Discord reports the source message unknown (error 10008, on the post or on a follow-up check
    after a 400/404), the remaining parts for that request are discarded and logged; nothing is
    posted unlinked.
27. Any other failure leaves the part pending. The daemon retries pending parts on its next service
    tick (at most every 3 s) with the same nonce, under the backoff of rule 8, and logs a stuck
    delivery once per request, not on every tick.
28. At most one delivery is in progress per session; parts of different requests go in request order.
29. A completed outbox entry is the reply receipt: request (channel and message), ordered remote IDs,
    time. Report linking (another spec) reads it from here. Keep receipts at least 7 days.

### Read receipts and reactions (DISC-OUT-08)

30. When the daemon admits an owner message as a request, it adds the eyes reaction (U+1F440) to that
    exact message and records the mark. Failure to react is logged and never causes the message to
    be admitted again.
31. Sessions may add or remove the bot's own reaction with any emoji on a message in an allowed
    channel; emoji are URL-encoded in the reaction route. Adding a reaction to an owner request
    counts as its read mark.

### Owner obligations (DISC-OUT-09)

32. Every owner request admitted to the lead creates an obligation keyed by channel and message,
    holding the admission time and the first 500 UTF-16 units of the text. Duplicates are ignored.
33. An obligation is settled when both a read mark (30/31) and a confirmed linked reply (16 or 29)
    for that exact message exist, each no earlier than its admission. Posting in the channel, a
    reaction on another message, or an unlinked post do not settle it.
34. Obligations admitted more than 30 minutes ago are no longer reported, so a restart after a long
    outage does not dredge up old work. Keep at most the 200 newest obligations.
35. When the lead session is about to stop (stop hook of the extension API) and open obligations
    exist, the extension asks OMP to continue once with a short notice listing at most 5 open
    requests (channel, message, time, quoted text) and how many more exist. Each obligation can cause
    at most 3 such continuations; after that it is left alone and logged.
36. Lanes and subagents never receive this notice.
37. With rules 20-30 in place the notice fires only when a turn ended without a publishable answer
    (rule 22, error, abort, ended on a tool call). The daemon never invents an answer.

### Splitting and rendering (DISC-OUT-03, 04)

38. A part holds at most 2000 UTF-16 code units, including any fence lines added by the splitter.
39. The cut is the last blank line, else the last newline, else the last space found in the second
    half of the window; with none, cut at the limit. Two UTF-16 units that form one character stay
    together, and a code span is not split: the cut moves to where its backticks begin.
40. Code blocks follow Discord's own pairing: every run of three or more backticks toggles a block,
    wherever it appears. If a part ends inside a block, the part is closed with a fence line and the
    next part reopens the block with the same language tag (only if the tag is 1-32 characters of
    letters, digits and `_+-.#`). If the text right after the cut is the block's own closing fence,
    that fence is consumed instead of reopening.
41. A block opener with no code after it in the part moves whole to the next part. An indivisible
    backtick run longer than a part is sent literally in chunks.
42. Parts consisting only of whitespace are dropped; if all are, the text is refused as empty. Text
    that fits in one part comes out byte-identical.
43. Table rendering rewrites a GitHub-style pipe table (header row, delimiter row with the same cell
    count, body rows with that count) into a fenced block of padded columns, using display width
    (wide characters count two). Delimiter-row alignment is applied by padding; the rendered
    separator is plain dashes.
44. A table is left untouched if a rendered row would exceed 80 display columns, if any row is
    ambiguous (control characters, an unclosed inline code span, a fence inside the row, mismatched
    cell counts), or if the message would exceed 64 KiB after rendering. If any fence in the message
    is ambiguous the whole message is left untouched.
45. Pipes inside code blocks, inside inline code and escaped pipes are never cell borders. Lines that
    start a list, heading, quote or indented code are never table rows. Applying the rendering to
    its own output changes nothing.

### Threads (DISC-OUT-11, 12, 13)

46. Thread titles are cut to at most 100 characters and must not be empty.
47. Creating a thread in a text channel makes a public thread (Discord type 11) and optionally posts
    a first message into it (split as usual); the result names the thread and first message ID.
48. Creating a thread from an existing message is required for announcement channels (type 5) and
    allowed for text channels; tags are refused for it.
49. Creating a forum or media post sends title, the first part of the text as the starter message
    (the title if no text) and the applied tags in one request; remaining parts follow in the new
    thread. If the forum has the "require tag" flag and no tag is given, refuse before any request;
    the product never picks a tag itself. Tags are given by exact name or ID and each must match
    exactly one available tag; the error lists the forum's tag names.
50. Changing a thread may set title, replace the full tag set, archive or unarchive; at least one
    change is required.
51. Thread status "done" puts a check mark and a space in front of the current title; "working"
    removes any such prefixes. The title is fetched from Discord each time; no saved copy is ever
    written back. If the change would exceed 100 characters or empty the title, refuse instead of
    truncating. An unchanged title causes no write.
52. Only the lead may set thread status, and "done" is refused while any open lane is bound to that
    thread.
53. A lane may create or change a thread only through a request bound to the owner's original
    message (the source): it must be an owner message in the current intake scope, and the target
    must be that message's channel (its parent for creation; the source's own thread for changes).
54. Lane thread operations are journaled before the request is sent: source, lane, parent, a
    fingerprint of the requested operation, time, state (pending, created, confirmed), and any
    thread ID seen. The journal is written atomically and fsynced.
55. Replaying the same operation for the same source returns the confirmed thread without a request.
    A different operation for the same source, a second creation for the same lane, or a pending or
    created entry is refused with an explanation; nothing is recreated.
56. A created thread is accepted only if its parent is the expected parent, its owner is the bot, its
    type matches the parent kind, it is not journaled for another lane, and (for forums) it has a
    starter message ID. Otherwise the candidate ID is journaled and the outcome is uncertain.
57. A pending entry whose request was definitely rejected (4xx, not uncertain) is removed so the lane
    can try again. An unreadable journal blocks all lane thread operations until a human fixes it.
58. Lane thread operations are serialised (one at a time per daemon).

### Owner questions: forms (DISC-OUT-15)

59. The daemon converts an OMP form (HTML) into a plain question or refuses; refusal means the form
    is answered only locally and nothing is posted.
60. Refuse if the HTML exceeds 16 KiB, has more than 256 elements or nesting deeper than 24, uses an
    element outside a small allowlist of text, layout and form elements, has event-handler or style
    attributes, hidden or read-only fields, duplicate attributes or IDs, unknown entities, or
    malformed nesting.
61. Supported fields: text, textarea, number, hidden, radio, checkbox, select. At most 16 fields and
    32 options per field, option values unique, names starting with a letter and up to 64 letters,
    digits, `_` or `-`. Refuse any field whose name or autocomplete hint suggests a secret
    (password, token, API key, private key, credentials, one-time code, card data).
62. At least one field must be editable. Hidden fields keep their values and are returned unchanged.
    Defaults come from the form (checked or selected options, initial values; a single select with
    nothing selected defaults to its first enabled option).
63. The question lists the form's visible text and, per field: label, type, required or optional,
    limits, enabled choices with exact values, default, and whether a free "other" value is allowed.
    All shown text is neutralised so it cannot mention anyone, open code spans or inject Markdown.
    Refuse if this summary exceeds 1500 characters.
64. A form is "single choice" when exactly one editable field exists and it is a radio group or
    single select; such forms get buttons. It "accepts plain text" when its single editable field is
    text or textarea, or a single choice that allows an "other" value.

### Owner questions: exchange (DISC-OUT-16, 17)

65. The question is posted to its route (DISC-OUT-18) while OMP's local dialog stays open. Whichever
    answers first wins; the other is cancelled. Cancelling the OMP request (or session shutdown)
    cancels the Discord side.
66. The daemon first posts a short placeholder, then edits it into the question, so the question's
    message ID is known before any answer can arrive. Messages older than the question are ignored.
67. Single-choice forms get one button per enabled option (at most 25: 5 rows of 5). Labels over 80
    characters become a numbered label with the full text in the message body. The one pre-selected
    option, if any, is styled primary and marked recommended; others secondary. Button IDs are random
    per question and at most 100 characters. If the question with buttons would exceed 2000
    characters, post the text-only form instead.
68. Buttons are offered only when the daemon's Gateway session is ready to receive component
    interactions (the application has no interactions endpoint URL); otherwise text-only.
69. A press is accepted only if it is a component interaction for this application, on this question
    message in this channel, with a known button ID, by the owner, who is not a bot. The daemon
    acknowledges an accepted press with a deferred update within 2.5 s; the answer counts only if
    that acknowledgement succeeded. Anyone else gets an ephemeral refusal. While one accepted press
    is being acknowledged, further presses are ignored.
70. Text answers: the owner replies (Discord reply) to the question message with a value or a JSON map
    of field name to value. If the form accepts plain text, an ordinary owner message in the same
    channel also counts, but only if exactly one such question is open in that channel and the
    message is newer than the question; if two are open, the message goes to normal intake.
71. Intake hands matching owner messages to the open question before any other routing and does not
    also treat them as new requests. They still get the read receipt.
72. Validation: answers up to 2000 characters; JSON maps may name only editable fields; omitted fields
    keep defaults; values must be enabled choices (or one "other" value where allowed), numbers
    within min/max, text within length limits, required fields non-empty. A plain value may match an
    option's value or, uniquely, its visible label.
73. An invalid answer gets one reply to that answer explaining the problem and asking to try again;
    only one such correction is in flight at a time. The question stays open.
74. Recommended default: if exactly one option is pre-selected, the question says it will be used
    after 3 minutes without an answer. After 3 minutes with no accepted answer and no press being
    acknowledged, the daemon re-checks that the route is still valid, then returns the recommended
    value to OMP marked as timed out. Without a recommended option there is no timeout; the question
    waits until answered or cancelled.
75. On finish the question is edited (3 s budget, best effort) to remove buttons and append a status:
    answered, recommended choice used after the timeout, or closed. A failed edit is only logged.
76. Open questions live in daemon memory and do not survive a restart; on start the daemon edits any
    question it had left open to "closed" (see state section).

### Owner questions: routing (DISC-OUT-18)

77. A lane's question goes only to that lane's own bound Discord thread or channel, never to the
    lead's. A lane without a Discord binding asks locally only.
78. The lead's question goes to the lead's configured home channel if exactly one is configured;
    discovered or followed channels are never defaults; with several candidates it stays local.
79. The owner may switch Discord questions off per session, or pin a session to a specific channel.
80. No question is posted to a channel the daemon is not currently watching for the owner's answers.

### Permission requests (DISC-OUT-20)

81. When an OMP session in a Discord-originated run raises a confirm/permission request through
    rpc-ui and the owner has enabled remote approval, the daemon posts a reply to the originating
    owner message with a short description and two buttons, approve (success style) and deny
    (danger style). Button IDs carry a stamp derived from bot, message and request IDs.
82. A press counts only if all rule-69 checks pass and the stamp matches. An accepted press is
    acknowledged by updating the message to remove the buttons; any other press gets an ephemeral
    refusal. Interaction callbacks use the interaction token and have a 2.5 s budget.
83. If the card cannot be posted, the request is denied (fail closed) and the run is cancelled. When
    the run ends for any reason, buttons on an unanswered card are removed and it counts as denied.

### Presence and avatar (DISC-OUT-21, 22)

84. The daemon's single Gateway session identifies with status online, no activity, not AFK, and
    re-sends that presence after a resume. No separate process or connection exists for presence.
85. At start-up, if the bot account has no avatar, the daemon uploads the bundled JPEG (must start
    with a JPEG signature and be at most 512 KiB) as a data URI. Failure logs one line and start-up
    continues. An existing avatar is never replaced.

## Third-party interfaces used

- Discord REST v10: create message (content, message_reference with fail_if_not_exists,
  allowed_mentions, nonce, enforce_nonce, components), edit message, get message, get channel, list
  guild channels, the bot's own reactions (PUT/DELETE, URL-encoded emoji), start thread without
  message, start thread from message, forum/media post with starter message and applied_tags,
  modify channel (name, archived, applied_tags), modify current user (avatar), get current
  application (flags, interactions_endpoint_url), interaction callback (type 4 with ephemeral flag
  64, type 6 deferred update, type 7 update message).
- Discord facts relied on: 2000-character content limit; message type 19 is a reply; error 10008 is
  unknown message; channel types 0, 5, 10-12, 15, 16; thread names up to 100 characters; forum
  "require tag" flag; buttons: 5 per action row, 5 rows, label 80, custom ID 100; interaction
  responses due within 3 s; components arrive over the Gateway only without an interactions endpoint
  URL; 429 bodies carry retry_after, responses carry X-RateLimit headers and a global flag; enforced
  nonces deduplicate recent creates.
- Discord Gateway v10: presence in identify and presence update (op 3); INTERACTION_CREATE
  dispatches (connection lifecycle is in the intake spec).
- OMP: rpc-ui form and confirm requests and their cancellation; session events for turn start/end
  and final messages; session files to confirm persistence; extension API stop hook with a
  continue-with-context result.

## State that must survive a restart

- Reply outbox per request: source channel and message, target session, ordered parts with text and
  index, per-part remote ID once confirmed, completion time. Completed entries are the reply
  receipts (keep at least 7 days).
- Read marks: channel, message, time (at least as long as obligations are reported).
- Obligations: channel, message, admission time, first 500 units of text, continuation count; the
  200 newest.
- Lane thread journal: per source and lane, operation fingerprint, state, thread ID, starter message
  ID, candidate ID.
- Open question and permission-card message IDs (channel, message), so a restarted daemon can close
  them.

All of this belongs in one private store owned by the daemon (one writer), written atomically.

## Problems in the old approach, and better ideas

- **Several processes for one bot.** Intake, presence, the question button listener and a lane
  monitor each open their own Gateway or REST client with the same token; the button listener even
  opens a fresh Gateway session per question. One daemon with one Gateway session and one REST
  client does all of it.
- **Duplicated logic in TS and Rust.** Splitting, posting, retry policy and reply bookkeeping exist
  twice with different limits (one retries 429 once up to 5 s, the other up to five times within
  60 s). One implementation, one policy.
- **Model-driven replies.** The lead model must run a send command and a react command for every
  owner message, and a stop-hook reminder nags it when it forgets. Letting the daemon post the final
  answer and add the read receipt removes most of that machinery and an extra model turn per request.
- **File protocols between processes.** Questions use heartbeat files with token digests and process
  start identities to find a live reader, answer files published by hard links, a plain-text claim
  window renewed every 2 s, and a relay over stdin IPC. The intake health record uses a locked
  directory with process identity checks. All of it disappears when one process owns the state.
- **Patching the OMP UI object.** The question mirror replaces the dialog function on the UI object at
  run time. Use rpc-ui form requests directly; the daemon answers the request when Discord wins.
- **Polling.** The question side polls its answer folder every 250 ms and the lane reply poster ticks
  every 3 s. In-process channels make both event-driven.
- **Nonces only on some paths.** Plain sends cannot be retried safely, so every uncertain write
  becomes a human task. Deterministic enforced nonces on every create make most retries safe.
- **Language detection by script.** Fixed strings switch to Korean when Hangul appears. A configured
  owner language is simpler and predictable.
- **Hard-coded demo approval.** The only button approval asks to read one specific file. Generalise
  to OMP's confirm requests or drop it.
- **Dead code.** A typing indicator is implemented but unused.

## Questions for the owner

1. Should the daemon always post a session's final answer itself (lead and lanes), so the model no
   longer sends replies and reactions by command and the stop-hook reminder becomes a rare fallback?
2. Question timing: keep the 3-minute automatic use of a single recommended choice, and should a
   question without a recommendation ever time out (and with what answer)?
3. Remote approval of OMP permission requests from Discord: enable it in general (fail closed on any
   doubt), or drop button approvals entirely?
4. Owner language for fixed bot strings: one configured language, or follow the question's language?
5. Should table rendering be always on for posted answers, or stay opt-in?
