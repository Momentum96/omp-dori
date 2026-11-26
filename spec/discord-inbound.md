# Discord intake: behaviour spec

Behaviour spec used to write Cataly 0.2; see docs/provenance.md.

## Purpose

The owner talks to their agents through Discord. This area covers the path from a message the
owner types in Discord to the moment that message is sitting, exactly once, in the right OMP
session: the lead session by default, or the session that owns a particular thread. It covers
receiving (live connection and catch-up reads), deciding which messages count, remembering how
far each conversation has been read, handing the message to a session, optional extra lines
attached to the message (channel facts, earlier-conversation lookup), and what happens around
crashes, rate limits and restarts. Sending replies, creating threads, approval forms and bot
presence are specified elsewhere.

Users: the owner (author of messages), the lead session and thread sessions (readers), and the
operator who configures the daemon (in practice the same person).

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| DISC-IN-01 | Bot and owner identity check at start | daemon start | KEEP | Fails closed before any message is read; cheap. |
| DISC-IN-02 | Watch scope: listed channels, DMs, threads under listed channels | every message | SIMPLIFY | Keep the rule, drop the three overlapping ways the old code computed it. |
| DISC-IN-03 | All-channels mode (every text channel in every guild plus the owner DM) | operator | DROP | Widens authority and REST cost; listed channels plus threads cover real use. See questions. |
| DISC-IN-04 | Owner authentication of each message | every message | KEEP | The only authority check on inbound text; must be strict. |
| DISC-IN-05 | Live Gateway intake | daemon | SIMPLIFY | Keep; share one Gateway connection with presence and button clicks instead of several. |
| DISC-IN-06 | Durable arrival queue | daemon | SIMPLIFY | Keep persist-before-handle; drop non-owner rows, raw-payload parity and fingerprint stalls. |
| DISC-IN-07 | Per-conversation read position and catch-up read over REST | daemon | KEEP | The recovery path for every gap in the live stream. |
| DISC-IN-08 | First-run baseline and start point for new threads | daemon | KEEP | Prevents replaying old history and prevents losing messages in new threads. |
| DISC-IN-09 | REST-only polling mode | operator | DROP | Slow wake, constant REST load; catch-up reads remain only as recovery. |
| DISC-IN-10 | Old-layout read-position import and the mode where intake ran its own OMP lead | migration | DROP | Compatibility layer; the new product has one mode. |
| DISC-IN-11 | Thread following and adoption | every thread message | SIMPLIFY | Keep behaviour; drive it from Gateway thread events, list active threads only on connect. |
| DISC-IN-12 | Message normalisation (text, reply, attachments, voice) | every message | KEEP | Sessions need a stable, data-only envelope. |
| DISC-IN-13 | Routing order: approval reply, then thread session, then lead | every message | KEEP | Clear precedence; each step is cheap. |
| DISC-IN-14 | Delivery to the session that owns a thread | thread sessions | SIMPLIFY | Keep the ownership rule; replace file locks and pane probing with in-daemon state. |
| DISC-IN-15 | Exactly-once hand-off to the lead | lead session | SIMPLIFY | Keep the guarantee; replace the multi-process ledger protocol and global stall. |
| DISC-IN-16 | Oversize message spill to a file | lead, thread sessions | KEEP | Bounds what enters a session. |
| DISC-IN-17 | Channel facts line (names, topic, owner's note) | sessions | KEEP | Optional, bounded, never blocks delivery. |
| DISC-IN-18 | Earlier-conversation lookup line | sessions | SIMPLIFY | Optional; keep the idea, drop hand-ported regex and Unicode tables. |
| DISC-IN-19 | Seen reaction after hand-off to a non-lead route | owner | KEEP | Visible feedback; failure is harmless. |
| DISC-IN-20 | Intake health state | operator, status | SIMPLIFY | Keep the definition of healthy; publish it from the daemon, not a separate record file. |
| DISC-IN-21 | Reply obligation recorded for lead-bound messages | outbound | MERGE into discord-outbound reply tracking | Belongs with sending; intake only signals "message delivered". |
| DISC-IN-22 | Proof that an action came from a specific owner message | thread and note commands | KEEP | Lets the lead act "on behalf of" a message without trusting its own claim. |
| DISC-IN-23 | Rate-limit and retry policy for REST and Gateway | daemon | KEEP | Required by Discord; bounded and predictable. |
| DISC-IN-24 | Private, crash-safe state storage | daemon | SIMPLIFY | One private database instead of per-channel files plus two databases. |
| DISC-IN-25 | Single-intake lock and bot/owner binding of state | daemon | SIMPLIFY | Keep; use one OS file lock. |
| DISC-IN-26 | Per-platform process identity readers (start time, boot id) | old cross-process checks | DROP | Only needed because several processes judged each other; one daemon does not. |
| DISC-IN-27 | Short-lived discovery file advertising the live intake to the extension | old ASK routing | DROP | Exists only to bridge processes. |
| DISC-IN-28 | Supervisor pipe on standard input and helper routing child process | old extension | DROP | Replaced by the daemon owning both sides. |
| DISC-IN-29 | Second intake implementation in the other language, with byte-for-byte parity code | old tree | DROP | Two implementations of one concern. |
| DISC-IN-30 | Uploading a default bot avatar when intake starts | operator | DROP | Not intake; the presence spec may keep it. |

## Requirements

### Start-up and identity (DISC-IN-01, DISC-IN-25)

1. At start the daemon reads the bot identity with the REST "current user" call. It refuses to
   start intake when the token is not a bot account, when the bot ID equals the configured
   owner ID, or when the configured owner ID is not a valid Discord snowflake.
2. It reads the current application and refuses to start intake unless one of the two
   Message Content intent flags (bit 18 or bit 19 of the application flags) is set. The error
   tells the operator to enable Message Content Intent in the Developer Portal.
3. Intake state is bound to the pair (bot ID, owner ID) recorded on first use. If either
   changes, intake refuses to start until the operator explicitly resets intake state. It never
   reuses read positions or queued messages from another pair.
4. Only one intake may run per state directory. The daemon takes an exclusive OS file lock on
   start; a second instance fails at once with a clear message. The lock dies with the process,
   so no stale-owner detection is needed.

### Scope (DISC-IN-02, DISC-IN-11)

5. The operator lists channels by ID (guild text channels, announcement channels, the owner's
   DM channel with the bot, forum or media channels, or individual threads). Duplicates are
   removed; at most 256 listed channels are accepted; any invalid ID fails start-up.
6. A message is in scope when its channel is listed, or when thread following is on (default on)
   and the channel is a thread whose parent is a listed non-thread channel.
7. Forum and media channels hold no messages of their own; listing one means "follow its
   threads". A thread's parent check uses only the thread's own parent field, never a category.
8. On every Gateway READY or RESUMED, and at start, the daemon lists active threads for each
   guild that has a listed parent. A thread that is not archived and whose parent is listed
   becomes followed. If more than 128 such threads exist, following is held for that round and
   reported, and only listed channels are read.
9. Gateway THREAD_CREATE adds a followed thread when its parent is listed. THREAD_UPDATE with
   the archived flag set, and THREAD_DELETE, stop following it. Stopping never deletes its read
   position or its queued messages; queued messages for an archived thread are still delivered.
10. A message arriving from an unknown thread triggers one bounded channel lookup (5 s). If the
    channel is a thread under a listed parent it is adopted at once. Lookup results, positive or
    negative, are cached for at most 256 threads, least recently used first out.
11. A listed channel that returns 403 or 404 fails start-up with a message naming the missing
    permission (View Channel, Read Message History). A followed thread that returns 403 or 404 is
    silently dropped from scope until it shows up again in an active-thread listing.

### Owner authentication (DISC-IN-04)

12. A message counts only when all hold: author ID equals the owner ID; the author is not a bot
    and not a system user; there is no webhook ID; the message type is 0 (default) or 19 (reply).
    Everything else in scope is ignored for delivery but still moves the read position past it.
13. An owner message whose content field is absent means the intent is missing; intake stops
    that conversation with an actionable error rather than delivering empty text.
14. An owner message whose text is empty after trimming and that has no attachments is skipped.
15. Message edits (MESSAGE_UPDATE) and deletions are ignored. What was first read is what is
    delivered.

### Live Gateway intake (DISC-IN-05, DISC-IN-23)

16. The daemon holds one Gateway connection (API v10, JSON encoding). It identifies with the
    intents for guild messages, direct messages and message content, plus guilds when thread
    following is on. The same connection serves presence and button-click events for the other
    areas.
17. Bounds: socket connect 10 s; HELLO within 10 s; READY or RESUMED within 20 s of HELLO;
    each outgoing frame written within 3 s; incoming frames over 1 MiB close the connection.
    A HELLO heartbeat interval outside 0.1 s to 120 s is treated as a protocol fault.
18. The first heartbeat is sent after a random fraction of the interval, then every interval. If
    no heartbeat ACK arrived before the next beat is due, the connection is dropped and resumed.
    A server heartbeat request (op 1) is answered at once.
19. The resume URL from READY is accepted only if it is `wss`, its host ends in `discord.gg`, and
    it carries no user info. Otherwise the connection is treated as faulty and re-identified.
20. Reconnect rules: op 7 resumes immediately. Op 9 with `d = true` resumes, otherwise forgets
    the session and identifies again; both wait a random 1 to 5 s. Close codes 4004 and 4010 to
    4014 stop intake and report the code's meaning (bad token, bad shard, bad version, bad or
    disallowed intents). Close 4008 waits at least 60 s. Closes 1000, 1001, 4003, 4005, 4007
    and 4009 forget the session. Any other close or socket error resumes if it can.
21. Backoff starts at 1 s and doubles to a 30 s ceiling; it resets once a connection has been
    established and acknowledged for 60 s. A fresh identify is never sent sooner than 6 s after
    the previous one.
22. A MESSAGE_CREATE is acted on only after READY, or while a resume is replaying missed events.
    For an in-scope owner message, the daemon writes it to the arrival queue (DISC-IN-06) before
    doing anything else, then wakes the handler. Persisting is synchronous with the event; the
    heartbeat is never blocked by delivery work.
23. On every READY or RESUMED the daemon durably records that a catch-up read is owed, refreshes
    the thread list (req. 8), then performs the catch-up read (req. 31) for every conversation in
    scope. The flag is cleared only when that read finishes. A crash before then repeats it.

### Arrival queue (DISC-IN-06)

24. Each queued item holds the raw owner message (at most 1 MiB), the conversation's read
    position at the moment of queueing, and the parent channel if the message came from a
    followed thread. The whole queue is capped at 256 MiB; when full, a new live event is not
    queued, a catch-up read is marked owed (req. 23), and that read recovers the message.
25. The queue is keyed by (channel ID, message ID). A second copy of the same key is ignored;
    the first copy wins, including its queue position. A later copy with different content is
    logged once and otherwise ignored; it never halts intake.
26. The handler takes items oldest first by arrival. Within one conversation it always handles
    lower message IDs first. A queued item whose conversation has left scope is dropped.
27. A queued message is delivered if it is not already recorded as delivered (req. 49) and it is
    newer than the read position stored with it. This covers a message seen live that a catch-up
    read missed because REST had not caught up yet.
28. On start, if the queue is not empty, the daemon performs a full catch-up read first and then
    drains the queue.

### Read positions and catch-up reads (DISC-IN-07, DISC-IN-08)

29. Every conversation in scope has a read position: the highest message ID fully handled. It
    never moves backwards. It moves past an owner message only after that message's hand-off is
    confirmed (req. 48), and past non-owner messages freely.
30. First run for a listed channel: the read position is set to its newest message ID (or "empty"
    if it has none), so history is never replayed. If queued messages already exist for a channel
    without a position, the position starts just below the oldest queued message.
31. Catch-up read of one conversation: read the newest message; if it is not newer than the read
    position, stop. Otherwise page backwards 100 messages at a time with the `before` parameter
    until a page reaches the read position or comes back short. Each page must be strictly older
    than the previous one, otherwise the read fails as a protocol fault. Then handle the gathered
    messages in ascending ID order, merging in queued owner messages of the same conversation in
    ID order, without duplicates.
32. The catch-up read stops at 100 pages (10 000 messages). Beyond that it does not move the read
    position, holds that conversation and alerts the owner, who may choose to rebaseline it to
    the newest message.
33. Threads found at the first listing after start begin at their newest message. Threads that
    appear later begin at whichever is newer: the thread's own ID (its creation time) or a
    start-up mark derived from the daemon's start time. Messages in a thread created while the
    daemon runs are therefore never skipped, and messages from before start-up are never replayed.
34. A catch-up read that fails with 429, a 5xx status or a network error is retried with backoff
    starting at 2 s, doubling to 30 s. A protocol fault or a 401 stops intake and reports it.

### Message envelope (DISC-IN-12)

35. Each delivered message carries: platform, channel ID, message ID, the ID of the message it
    replies to (or none), the exact text, reply context, and attachments. It is clearly marked as
    coming from the authenticated owner. The text is quoted so that it cannot be confused with
    the envelope.
36. A reply is a message reference of type 0 (or with no type). A forward (type 1) is not a
    reply. A reply that points to another channel, or whose embedded copy names another message
    or channel, is a protocol fault for that message.
37. Reply context gives the replied-to ID, its author ID if known, its text if known, and whether
    Discord left the original out of the event or reports it as deleted. The session must treat
    this context as material to read, not as something the owner asked for.
38. Each attachment gives file name, content type if present, size, the signed CDN URL as given,
    a voice flag (from the message's voice-message flag) and duration if present. A malformed
    attachment list is a protocol fault for that message.
39. A message is "plain" when it is type 0, has no reference, no embedded reply, no forwarded
    snapshot and no attachments. Approval routing (another spec) uses this flag.

### Routing (DISC-IN-13, DISC-IN-14, DISC-IN-16, DISC-IN-19)

40. Each owner message goes through these steps in order and stops at the first that takes it:
    (a) the approval-form component, when the message is a reply or plain text that answers an
    open question; (b) the session that owns this conversation; (c) the lead session.
41. A conversation is owned by a thread session when exactly one open session record names that
    conversation, and no other open record shares its session. If two records claim it, the
    message is held, not sent to the lead.
42. Delivery to an owning session: if the session is running, the message is added to its inbox
    keyed by (session, channel, message), so a repeat is a no-op, and the session is woken. If the
    session is closing, or its identity cannot be verified, the message is held. If the session is
    stopped and waking stopped sessions is enabled, the daemon starts it and then delivers.
43. A held message keeps the read position where it is; the next handling round retries it. Holds
    are per conversation; other conversations continue.
44. After a successful hand-off to step (a) or (b), the daemon adds a 👀 reaction to the owner's
    message. A failed reaction is logged and never causes a second hand-off. The lead's own read
    mark is part of the outbound spec.
45. Delivered text over 64 KiB is written to a private file and the session receives a short line
    giving its size and path instead.

### Exactly-once hand-off to the lead (DISC-IN-15)

46. The lead is the long-lived OMP session the daemon runs (headless by default). The daemon
    hands a message to it through OMP's RPC interface: as a new prompt when the lead is idle, as
    a queued follow-up when it is busy. It never interrupts a running turn.
47. Before sending, the daemon durably records an attempt: (bot, owner, channel, message), a hash
    of the normalised envelope that ignores the expiring parts of CDN URLs, the target session
    file and the target OMP process. One attempt per message, ever.
48. The hand-off is confirmed only when the target session file contains a user entry carrying
    that channel and message ID. A returned RPC call alone is not confirmation. The daemon waits
    up to 30 s for the entry to appear; after that the attempt is in doubt and req. 50 applies
    without waiting for a restart.
49. On confirmation, in one transaction: the attempt is marked delivered, the read position moves
    to the message, and the queue item is removed. A hand-off to a thread session's inbox uses
    the same single transaction for inbox insert, read position and queue removal.
50. When an attempt is in doubt (found unconfirmed at start, or after the wait in req. 48), the
    daemon searches the recorded session file from the end for the entry. Found: confirm (req.
    49). Not found and the recorded OMP process is gone: the file is final, so the message was
    not delivered; send it again.
    Not found and the process is alive: ask it over RPC to finish writing, then search again once.
    Still in doubt: hold that conversation only, and tell the owner which message is in doubt and
    how to release it.
51. An entry with the same channel and message ID but a different envelope hash is a conflict:
    the conversation is held and reported, never silently resent.
52. Delivered records are kept for 7 days or until the read position has passed the message and
    no queue item remains, whichever is later.

### Extra lines (DISC-IN-17, DISC-IN-18)

53. When the operator enables it, lead-bound messages get one extra line of channel facts: for a
    channel, its ID, name, topic (if not empty) and the owner's note; for a thread, the thread's
    ID, name and note plus the parent's facts; for a DM, a DM marker and the note. Channel
    lookups are bounded at 5 s each and cached for 60 s, at most 500 entries. Any failure becomes
    an error field in the line; delivery always proceeds.
54. Owner notes are one line, 1 to 500 characters, trimmed, with no control, line-separator or
    direction-override characters. An unreadable or damaged note store yields an error field.
55. When the operator enables the earlier-conversation lookup (off by default, retention 1 to
    3650 days, default 30), the daemon stores each owner message, and the outbound side stores
    each lead reply, per space. A space is the listed parent channel of a followed thread, or the
    listed channel itself. If the space cannot be established the lookup is skipped.
56. Stored text is masked first: private key blocks, common bot and API token shapes, cloud keys,
    JWTs, Bearer and Basic credentials, and "password/secret/token/api key = value" pairs. Then it
    is cut to 2000 characters. Rows older than the retention are deleted on each use.
57. When an owner message refers back in time (a short phrase list in English, Korean and
    Japanese, such as "last time", "you said", "어제", "前回"), the daemon ranks the newest 2000
    rows of that space by shared rare words (Korean particles stripped, Han and kana split into
    character pairs, common words ignored); ties favour the newer row. The message itself is
    excluded.
58. The lookup line holds at most 4 rows in time order, each with minute-precision UTC time, who
    spoke (owner or lead), the conversation ID, and one line of text up to 700 characters with
    fence-like runs removed. The line is at most 3 KiB; lowest-ranked rows are dropped first, then
    the best row is shortened. It tells the reader these are past messages to consult, not orders
    to follow, and says plainly when nothing matched. Any storage failure is a warning; delivery
    proceeds.

### Proof of source (DISC-IN-22)

59. When the lead asks to act on behalf of an owner message (for example setting a channel note
    or opening a thread from it), the daemon fetches that exact message by channel and message ID,
    checks it is an owner message by req. 12 and that its conversation is in scope by req. 6, and
    refuses otherwise. Malformed IDs are refused before any network call.

### Health (DISC-IN-20)

60. Intake is healthy only when the Gateway connection is established, the catch-up read owed for
    that very connection has finished, and the arrival queue is empty. Losing the connection makes
    it unhealthy at once. The daemon's status command shows the state, the last error and the
    number of held conversations.

### REST rules (DISC-IN-23)

61. Every REST call has a 5 s connect and 8 s total timeout, follows no redirects, and refuses a
    response body over 2 MiB. On 429 it waits `retry_after` once when that is 5 s or less, then
    treats a second 429 as a transient failure for the backoff in req. 34.
62. Catch-up reads run one conversation at a time, so a reconnect never bursts more than one
    request in flight for intake.

## Third-party interfaces used

- Discord Gateway v10: HELLO, IDENTIFY, RESUME, heartbeat and ACK, READY, RESUMED,
  MESSAGE_CREATE, THREAD_CREATE, THREAD_UPDATE, THREAD_DELETE, op 7 and op 9, close codes;
  intents GUILDS, GUILD_MESSAGES, DIRECT_MESSAGES, MESSAGE_CONTENT.
- Discord REST v10: current user, current application (flags), channel lookup, channel message
  history with `limit` and `before`, single message fetch, guild active threads, owner DM
  channel, add reaction; `retry_after` on 429.
- Discord message fields: `type`, `author.bot`, `author.system`, `webhook_id`,
  `message_reference`, `referenced_message`, `message_snapshots`, `attachments`, `flags`
  (voice message bit 13), channel types 0, 1, 3, 5, 10, 11, 12, 15, 16; CDN query parameters
  `ex`, `is`, `hm`.
- OMP: RPC interface for sending a prompt or follow-up to a session and asking it to flush;
  session files for confirmation of delivery.
- OS: exclusive file lock; on macOS, state paths are resolved to their real location first,
  because the temporary directory sits behind a symlink.

## State that must survive a restart

- The (bot ID, owner ID) pair the state belongs to.
- Per conversation: read position, parent channel if a followed thread, and held status.
- The arrival queue with each item's read position at arrival and parent channel.
- Whether a catch-up read is owed.
- Delivery attempts and delivered records (identity, envelope hash, target session file and
  process, outcome) within the retention of req. 52.
- Thread-session inbox items not yet taken by their session.
- Owner notes per channel or thread.
- Earlier-conversation rows when that feature is on.
- Nothing else. Thread lists, caches and health are rebuilt.

## Problems in the old approach, and better ideas

- Three intakes for one job: one in TypeScript, a Rust copy of it kept byte-identical (including
  hand-ported JavaScript regex, number formatting and Unicode tables), and an older Rust mode that
  ran its own lead. Build one Rust intake.
- The intake was a child process of the OMP extension, talking over an inherited socket. The
  extension spawned a further Rust helper process just to scan session files. Approval routing
  needed a short-lived discovery file to find the live intake. A single daemon removes all three.
- REST polling (a probe request per channel per interval) was a full mode, and thread-session
  waking worked only in that mode. Use the Gateway always; keep REST for catch-up only.
- Presence used its own Gateway connection beside intake's. One connection is enough.
- Read positions lived in one small file per channel, the queue and the hand-off ledger in a
  database, the earlier-conversation rows in another database, and notes in a JSON file. The
  windows between them needed checkpoint flags and a delete trigger. One database with one
  transaction per hand-off removes those windows.
- Every message in scope was queued, owner or not. Queue only owner messages.
- One in-doubt hand-off stopped every conversation until the operator abandoned it, and
  abandonment needed proof that the old lead process was dead. Hold only that conversation, and
  resolve automatically when the session file is final.
- A "rejected" hand-off outcome existed only because lead delivery went through a line filter
  meant for monitor output. Not needed.
- A duplicate live event with changed content raised a hard error. Keep the first copy and log.
- A backlog beyond the page bound held the conversation forever. Alert and offer a rebaseline.
- Process identity was read from `/proc` on Linux and from libproc plus `sysctl` on macOS so
  processes could judge each other's liveness. A file lock makes this unnecessary.

## Questions for the owner

1. Drop the all-channels mode (req. 5 replaces it with listed channels plus their threads)?
2. Keep the earlier-conversation lookup (opt-in) or drop it and rely on OMP's own memory?
3. When a lead hand-off is in doubt and the lead process is gone, may the daemon resend
   automatically (req. 50), or must the owner always confirm?
4. When the lead is busy, should owner messages queue as follow-ups (req. 46) or steer the turn?
5. If the Gateway rejects the intents, should intake fall back to slow REST reads or stay down?
