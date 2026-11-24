# Telegram, Slack and voice: behaviour spec

## Purpose

The owner talks to the agent from a chat app or by speaking. This spec covers:

- Telegram: a bot that takes the owner's messages from a direct chat or one forum topic, marks
  them as seen, and posts or edits answers.
- Slack: a workspace account (an app or a member) that takes messages addressed to it, including
  untagged replies in threads it posted in, and posts, edits or uploads files.
- Voice: the owner joins a Discord voice channel and talks to a realtime speech model that can
  read status, search the web, leave a text note and pass work to the lead OMP session; the
  lead's answers are read back aloud. Spending is capped. Plus turning an audio file into text.
- The shared messenger model that Discord, Telegram and Slack all use.

Users: the owner, the lead OMP session, worker sessions (lanes), and the daemon, which alone holds
every credential and connection. Discord text intake and output have their own specs.

## Capabilities

### Shared messenger model

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| MSG-01 | One normalised inbound event for all platforms | daemon, lead | KEEP | One intake and routing path for all three. |
| MSG-02 | Per-platform sender gate, fail closed | daemon | KEEP | Only authorised people may wake the agent. |
| MSG-03 | At-least-once intake: commit position only after hand-off | daemon | KEEP | No lost owner message across crashes. |
| MSG-04 | Seen receipt (reaction) on admitted new messages | owner | KEEP | Owner relies on it to know a message was taken. |
| MSG-05 | One send request shape with confirmed / rejected / uncertain outcome | sessions | KEEP | Prevents duplicate posts on every platform. |
| MSG-06 | One HTTP policy: deadlines, read retries, rate limits, no blind write retry | all | SIMPLIFY | Old policy had no request deadlines and per-platform special cases. |
| MSG-07 | Positional text lines on standard output as the intake contract | lead, monitors | DROP | Exists only because intake ran as child processes. |
| MSG-08 | Refuse text containing a shell substitution marker | sends | DROP | Symptom fix for shell-built text; structured calls cannot have it. |
| MSG-09 | File-based intake health record with process identity | updater | DROP | The daemon knows its own health. |
| MSG-10 | Typing-indicator helper | none | DROP | Never called. |
| MSG-11 | Set bot name / avatar when empty, with opt-out | daemon | KEEP | One cheap startup step, as for Discord. |

### Telegram

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| TELE-01 | Startup checks: bot identity, no webhook, writable state | daemon | KEEP | Prevents consuming another consumer's updates. |
| TELE-02 | Long polling for new and edited messages | daemon | KEEP | Needs no public endpoint. |
| TELE-03 | Durable offset, 24-hour validity rule, small replay guard | daemon | SIMPLIFY | Keep the rules; use message identity instead of content hashes. |
| TELE-04 | Owner gate: DM, optional group, optional topic | daemon | KEEP | Core safety rule. |
| TELE-05 | Normalise message: kind, topic, reply target, media handle, text | daemon | KEEP | Needed to route and answer in place. |
| TELE-06 | Drop live-location refresh edits | daemon | KEEP | One shared location would flood the lead. |
| TELE-07 | Receive-lag metering and percentiles | owner | SIMPLIFY | Keep numbers in the daemon; drop the journal file and command. |
| TELE-08 | Send plain text, split at 4096 units | sessions | KEEP | Core. |
| TELE-09 | Edit one message in place | sessions | KEEP | Status messages and corrections. |
| TELE-10 | Single-request HTML send | sessions | SIMPLIFY | Keep as a send option; the table helper had no caller. |
| TELE-11 | Forum topic create / rename / close / reopen | none | DROP | Never called. |
| TELE-12 | Streaming draft with placeholder; typing action | none | DROP | Never called. |
| TELE-13 | One-shot single-poll mode | scripts | DROP | The daemon polls continuously. |
| TELE-14 | Telegram intake on Windows (native ACL store, lease) | Windows | DROP | Owner decision. |
| TELE-15 | Conversation recall for Telegram turns | lead | MERGE into the recall spec | Not a messenger concern. |

### Slack

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| SLACK-01 | Credential kinds: app (bot) token or member token | owner | SIMPLIFY | Both roles through one documented app; drop the browser-cookie session. |
| SLACK-02 | Protected token file; home-bound path handed to lanes | daemon, lanes | SIMPLIFY | Only the daemon reads it; lanes never need path or token. |
| SLACK-03 | Account identity check at start | daemon | KEEP | Binds positions and routes to one account. |
| SLACK-04 | Destinations qualified by workspace and account | sessions | SIMPLIFY | Always qualified internally; drop the dual output format. |
| SLACK-05 | Intake by polling web-client Threads view, thread memory, unread counts | daemon | SIMPLIFY | Documented events plus bounded catch-up instead. |
| SLACK-06 | Memory of threads the account posted in, with seen positions | daemon | KEEP | Untagged replies in those threads must arrive. |
| SLACK-07 | Skip own and bot messages; dedupe; reject inconsistent thread data | daemon | KEEP | Avoids loops and misrouting. |
| SLACK-08 | Accept any non-self human sender | daemon | SIMPLIFY | Add an allowlist; old intake had no gate. |
| SLACK-09 | Post (optionally in a thread) and edit | sessions | KEEP | Core. |
| SLACK-10 | External file upload with strict ticket and URL checks | sessions | KEEP | Part of the shared send model; old code had no caller yet. |
| SLACK-11 | Keep a member shown online via a web-client socket | owner | DROP | Undocumented, needs a browser cookie, extra process. |
| SLACK-12 | Seen receipt on Slack | owner | KEEP (new) | Old tree had none; needed for the shared model. |

### Voice

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| VOICE-01 | Explicit opt-in settings with strict validation | owner | KEEP | Voice costs money; no implicit models or prices. |
| VOICE-02 | Single-instance lock for voice | daemon | MERGE into the daemon | One daemon owns one voice task. |
| VOICE-03 | Separate Discord Gateway connection for voice states | daemon | MERGE into the Discord intake connection | Add the voice-state intent to the one connection. |
| VOICE-04 | Join the voice channel; readiness deadline; rejoin; stop if moved | daemon | KEEP | Core. |
| VOICE-05 | Track whether the owner is in the channel | daemon | KEEP | Opens and closes sessions. |
| VOICE-06 | Receive only the owner's audio; resubscribe; stall alarm | daemon | KEEP | Discord receive streams die on bad packets. |
| VOICE-07 | Discord voice end-to-end encryption (DAVE), short plaintext window | daemon | KEEP | Discord requires it. |
| VOICE-08 | Format conversion, level meter, bounded playback, barge-in | daemon | KEEP | Core audio path. |
| VOICE-09 | Wake on speech with 2.5 s of lead-in audio | daemon | KEEP | Saves cost without losing first words. |
| VOICE-10 | Pace input audio to the provider's real-time limit | daemon | KEEP | Provider drops audio sent too fast. |
| VOICE-11 | Realtime speech session over WebSocket with delegated backend | daemon | KEEP | Core. |
| VOICE-12 | WebRTC transport and provider standby | owner | DROP | Second transport, untested on the paid service; idle close plus wake covers it. |
| VOICE-13 | Backend tool execution and response continuation | daemon | KEEP | Makes tools work. |
| VOICE-14 | Four tools: web search, status read, text note, pass work to lead | voice model | KEEP | Small, read-mostly, no shell. |
| VOICE-15 | Pass work to the lead with checked preconditions and receipt proof | lead | SIMPLIFY | Keep three outcomes; use OMP RPC instead of screen reading and file tailing. |
| VOICE-16 | Early partial status spoken while work is pending | owner | KEEP | Cuts perceived latency. |
| VOICE-17 | Lead replies read aloud, consumed only after proven playback | owner | SIMPLIFY | Replies via a daemon call, not a file path placed in a prompt. |
| VOICE-18 | Greeting, limit announcement, idle close | owner | KEEP | Session lifecycle. |
| VOICE-19 | Usage ledger and caps (daily, total, spending) | owner | KEEP | Spending guard. |
| VOICE-20 | Hold after any provider or safety failure; no paid retry | owner | KEEP | Fail closed on money. |
| VOICE-21 | End-of-session summary posted to a text channel | owner | KEEP | Written trail. |
| VOICE-22 | Model instructions with delivery-honesty rules | voice model | KEEP | Prevents false "done" claims; text must be newly written. |
| VOICE-23 | Lazy loading of optional native codec packages | install | DROP | The Rust build links them behind a build feature. |
| VOICE-24 | Transcribe an audio file or URL on request | lead | SIMPLIFY | Keep command and size cap; trim deep container inspection. |

Totals: KEEP 36, SIMPLIFY 12, MERGE 3, DROP 11 (62 capabilities).

## Requirements

### Shared model (MSG)

1. MSG-01 Each admitted message becomes one event: platform; account (bot ID, or workspace plus
   account); conversation; sub-conversation (Telegram topic, Slack thread root, Discord thread)
   or none; message ID; kind (new or edit); author; text (caption counts; may be empty); replied-to
   message or none; attachments (kind, platform handle, size if known); platform time; receive time.
2. MSG-01 Events of one conversation reach OMP in platform order. Platforms and conversations are
   independent: one stalled platform never delays another.
3. MSG-02 Each platform has an allowlist of sender IDs. Unlisted senders, bots and the account
   itself are dropped before any receipt or hand-off. An empty or invalid list disables that
   platform's intake at start with a clear error.
4. MSG-03 A position (offset, cursor, seen timestamp) is saved durably only after hand-off of that
   message to OMP succeeded; dropped messages advance it at once. A crash between hand-off and save
   may repeat a message, never lose one. Each event carries a stable dedupe key.
5. MSG-03 An unreadable or invalid saved position: start without it and say so once. A failed
   position write: stop that platform's intake, show it unhealthy, re-check every 60 s; never poll
   past an unsaved position.
6. MSG-04 Each admitted new message (not edits) gets one "eyes" reaction, sent in parallel with
   hand-off. A failure is logged once per platform per run; the message is still delivered.
7. MSG-05 A send names platform, account, conversation, optional sub-conversation, text, and
   optionally a reply target, an edit target or a file. Only the daemon performs sends.
8. MSG-05 Each send ends as exactly one of: confirmed (all created message IDs), rejected (nothing
   created), uncertain (may exist). A multi-part send stops at the first failure and reports the
   IDs already confirmed. Uncertain sends are never repeated automatically.
9. MSG-06 Deadlines: 15 s per ordinary request; long polls get their wait plus 10 s; file transfers
   and model summaries 60 s.
10. MSG-06 Reads (including named read-only methods sent as POST) retry up to 5 attempts on 429,
    5xx or timeout, waiting 1 s doubling to 60 s, or the server's requested delay.
11. MSG-06 Writes are never retried after a transport error, timeout, 408 or 5xx (uncertain). A 429
    asking for at most 60 s may be waited out and retried; longer returns rejected-with-delay.
12. MSG-06 A 2xx write whose body cannot be parsed or lacks the created ID is uncertain.
13. MSG-06 Logs and errors never contain a token, cookie or URL embedding one.
14. MSG-11 At start, an empty bot display name is set to the product name and a missing profile
    photo is replaced by the bundled avatar; 5 s per call; failures are logged once and intake
    continues. An owner setting turns this off.
15. Health: per platform, available or not, last successful poll or event, and receive-lag
    percentiles, through the daemon's status surface.

### Telegram (TELE)

16. TELE-01 Before polling, the bot identity must be a bot with a positive ID, and webhook info must
    show no webhook URL; otherwise intake does not start and says why. The webhook is never changed.
17. TELE-01 The position store must be writable before intake starts. Positions are bound to the
    bot ID: another bot's position is refused. Token rotation for the same bot keeps it.
18. TELE-02 Each poll asks for at most 100 updates, a 25 s wait, and only new and edited messages.
    The next poll starts immediately after each response; the long poll is the wait.
19. TELE-02 A response that is not an array, has over 100 items, or has an update ID that is not a
    non-negative safe integer is a protocol error: nothing is handed off; retry with backoff.
20. TELE-02 409 (another consumer polling this bot): unhealthy, log once, retry every 30 s. 401:
    stop Telegram intake until the token changes. A 429 delay from the body or header is honoured
    however long, while Telegram shows unhealthy.
21. TELE-03 The saved next-update offset is sent only if the last non-empty batch was seen less
    than 24 hours ago by the local clock. At exactly 24 hours, with no recorded time, or after the
    clock went backwards, it is left out. Empty batches do not renew the window.
22. TELE-03 The replay guard holds identities (chat, message ID, edit date) of handed-off messages
    that appear in the latest batch, at most 100; entries missing from a new batch are removed. A
    guarded message is skipped but still advances the offset.
23. TELE-03 After each update the daemon saves next offset, batch time and guard atomically (write,
    flush, rename, flush directory) in an owner-only file.
24. TELE-04 Kept only if the sender is not a bot and is the owner, and either the chat is private
    with ID equal to the owner's, or a group is configured, the chat is that group or supergroup,
    and no topic is configured or the message's topic equals it.
25. TELE-04 A message's topic is its thread ID only when Telegram marks it a topic message; in a
    group otherwise it is General, counted as topic 1. A reply's thread ID in General is a reply
    root, not a topic.
26. TELE-05 Edited-message updates are kind "edit" and keep the original message ID. The reply
    target is the replied-to ID, except a topic message pointing at its own topic root (no reply).
27. TELE-05 Attachment: the first present of voice, audio, video, round video, animation, document,
    photo (largest size). Only the file handle is carried; nothing is downloaded unless asked.
28. TELE-06 An edit carrying a location and no text or caption is dropped and advances the offset.
29. TELE-07 Receive lag = receive time minus Telegram's message date (original date for edits),
    clamped at 0; no date, no reading. The last 1000 readings give nearest-rank p50, p90, p99 and
    max on request. Recording never affects intake.
30. TELE-08 Text over 4096 UTF-16 units is split: cut after the last newline in the back half of
    the window, else the last space there, else at the limit, never inside a surrogate pair. The
    parts joined equal the input. Each part must return a positive message ID, else uncertain.
31. TELE-08 A topic, when given, must be a positive integer; never fall back silently to General.
    Sends go only to the owner's DM or the configured group/topic unless settings list another
    chat; others are refused before any request.
32. TELE-09 An edit over 4096 units is refused before any request; edits are never split or turned
    into a new post.
33. TELE-10 The HTML option sends one unsplit request and leaves markup validation to Telegram;
    HTML edits get the same 4096 preflight on the raw markup.

### Slack (SLACK)

34. SLACK-01 Two roles, chosen at setup: as an app (bot user; sees only conversations it is in) or
    as a member (user token of the same app; sees what that member sees; needs a paid seat). Both
    use one installed app in Socket Mode, so no public endpoint is needed.
35. SLACK-02 Tokens come from one file owned by the running user with no group or other bits,
    opened without following links and checked on the opened handle. Missing, empty, non-regular,
    foreign-owned or too-open files disable Slack and the error names only the path. On Windows
    the mode check is skipped with a one-time reminder that the ACL is the owner's job.
36. SLACK-03 At start the identity call must return a workspace ID and an account ID of letters,
    digits, dash or underscore, else Slack stays off. Startup gives it 10 s; on timeout Slack shows
    unhealthy and retries in the background.
37. SLACK-04 Every destination names workspace, account and channel (C, G or D prefix). A send for
    another workspace or account is refused before any request, including upload allocation.
38. SLACK-05 Live intake: Socket Mode events for DMs, group DMs, mentions and messages in
    conversations the account is in. Each envelope is acknowledged within 3 s, after the event is
    admitted or dropped. Reconnect backoff 1 s doubling to 60 s.
39. SLACK-05 After start and every reconnect: for each remembered thread read replies newer than
    its seen position (at most 200, oldest first); for each DM with a position read newer history.
    Dedupe key is channel plus message timestamp, shared with live events.
40. SLACK-06 Every post records its thread (channel, root timestamp) with seen position = the post's
    timestamp, keyed by workspace and account. An already recorded thread keeps its older position.
41. SLACK-06 Replies in remembered threads are admitted without a mention. Seen positions only move
    forward and are saved after hand-off.
42. SLACK-06 Unreadable thread memory stops Slack posting and intake without changing the file.
43. SLACK-07 Own and bot messages are dropped. A reply whose stated channel or root disagrees with
    where it was read, or one message seen with two different roots, stops that read before any
    hand-off.
44. SLACK-08 Only allowlisted senders are admitted; the owner may list several members.
45. SLACK-09 A post first checks thread memory is readable. The response must carry a channel and a
    digits.digits timestamp (and, for a qualified destination, the same channel), else uncertain.
    If the post is confirmed but recording the thread fails, the outcome is confirmed plus a
    local-tracking error; do not repost.
46. SLACK-09 An edit target must be digits.digits, else refused before any request. A 2xx with no
    boolean success field, or with an internal, fatal, timeout or unavailable error, is uncertain.
47. SLACK-10 Upload: request a ticket (name, byte length); its file ID must be "F" then upper-case
    letters or digits. The URL must be https on exactly files.slack.com, with no user or password
    and under Slack's upload path, else refuse before sending bytes. Send bytes without following
    redirects; require 200. Complete into the channel (optional thread, title defaults to the
    name); the completion must list the same file ID, else uncertain.
48. SLACK-12 Admitted new messages get the "eyes" reaction when the app has the reactions scope;
    without it receipts are skipped and status says so once.

### Voice (VOICE)

49. VOICE-01 Off unless enabled. Required: voice and text channel IDs (Discord snowflakes), speech,
    backend and summary models, voice name, per-million-token prices (input, cached input, cache
    write, output) for backend and summary models, and a per-search price (may be 0). The provider
    key comes from the daemon's secret store, never plain settings.
50. VOICE-01 Optional: backend reasoning effort (low); owner conversation preferences (empty); IANA
    time zone (UTC); daily cap 30 min (>0); total cap 0 = off (>=0); spending cap 3 USD (>0); idle
    close 120 s (>0); wake level -45 dBFS (strictly between -120 and 0); per-minute speech prices
    for the summary estimate only. Invalid settings refuse voice and name the field.
51. VOICE-03 The one Discord Gateway connection also requests guild and voice-state events; voice
    reads only the configured guild's snapshot and voice-state updates.
52. VOICE-04 At start the channel must be a guild voice channel and the owner ID must differ from
    the bot's. Join neither muted nor deafened.
53. VOICE-04 Any non-ready link state starts one 30 s readiness deadline for all recovery; missing
    it stops voice with an error. A disconnect lasting 5 s tears down and rejoins after 3 s. If the
    bot is moved to another channel, voice stops.
54. VOICE-04 Output is audible only while the Gateway is up, the bot is seen in the channel and the
    link is ready; losing any mutes, discards queued audio and marks the owner absent.
55. VOICE-05 The owner is present only while seen in the channel and output is audible. Present
    opens a session with greeting; absent closes it and clears lead-in audio.
56. VOICE-06 While the link is ready and the owner present, exactly one receive stream exists, for
    the owner only. Undecodable Opus frames are skipped. A closed stream is resubscribed at once if
    it lived over 10 s, else after a delay doubling from 100 ms to 2 s.
57. VOICE-06 Once a second, check whether Discord reports the owner as speaking while no audio has been decoded for 3 s; if so, restart
    the stream (log at most once per 10 s).
58. VOICE-07 After each encryption-group change, unencrypted owner frames are accepted for 30 s;
    otherwise dropped. Decryption failures are logged as one count per burst (1 s quiet ends it).
59. VOICE-08 Discord audio (48 kHz stereo 16-bit) is averaged down to 24 kHz mono; provider audio is
    interpolated back up across chunk edges and played in 20 ms frames. Playback holds at most 5 s;
    overflow clears it and fails the session. Each clear starts a new playback generation.
60. VOICE-08 Barge-in: owner above the wake level for 400 ms while over 400 ms is queued clears
    playback. Provider audio below -55 dBFS is dropped while over 200 ms is queued. After 1 s with
    no output, a partial last frame is padded with silence.
61. VOICE-09 With no session and the owner present, 200 ms above the wake level opens a session
    without greeting; the last 2.5 s of audio (120,000 bytes at 24 kHz mono) is sent first.
62. VOICE-10 Input goes through a token bucket refilling at 1.15x real time, holding at most 3 s,
    starting full. When over 100 ms behind real time, silence pads within the bucket. Over 3 s of
    pending input fails the session.
63. VOICE-11 States: starting, live, winding down at a limit, closing, closed. The opening event
    sets speech model, instructions, provider storage off, 24 kHz PCM, voice, and the backend model
    with instructions, tools, parallel tool calls and reasoning effort.
64. VOICE-11 Starting over 30 s closes with a timeout; closing over 15 s force-terminates the
    socket; only audio is sent while winding down. Provider errors close the session, except
    "context injection incomplete" or "context length exceeded" tied to a client event (logged).
    A malformed provider event is a provider failure.
65. VOICE-13 Backend events are matched to a delegation by ID, then response ID, then the single
    running candidate; unmatched ones are logged and ignored. Usage is recorded once per response,
    even after close or cap.
66. VOICE-13 A call runs when its item completes. Status reads run at once; other tools run one at
    a time in arrival order. Non-object arguments, a failed response or an ended session mean no
    execution and an explanatory failure. Outputs go back as function outputs; when every call is
    answered and nothing runs, the daemon asks the backend to continue.
67. VOICE-13 A running delegation silent for 120 s closes the session as a stall; unanswered calls
    are cancelled, never faked. Running work, pending calls and 10 s after finished work block
    idle close.
68. VOICE-14 Web search is the provider's hosted tool; each completed search adds its price.
69. VOICE-14 Status read takes a non-empty subset of {lane status, recent reports} and an early-
    update flag; anything else is refused. Lane status: every open lane with live state and since
    when, plus unregistered sessions, at most 3000 characters. Reports: newest report of up to 8
    lanes, 350 characters each, newest first. Each part has its read time and fails on its own.
70. VOICE-14 Text note: control characters except newline become spaces; at most 1800 characters;
    empty refused; posted to the text channel with a short prefix showing it came from voice.
71. VOICE-14 No voice tool runs a shell, writes outside daemon state, or logs its arguments.
72. VOICE-15 The request is cleaned to one line (control characters, backticks, dollar signs and
    backslashes become spaces; whitespace collapsed) and clipped to 700 characters; empty is
    refused before sending.
73. VOICE-15 Before sending, the lead must be a live OMP session known to the daemon with no pending
    approval or question and able to take input; otherwise it is refused with the reason, no retry.
74. VOICE-15 The sent text carries a fresh request ID, a voice-origin marker, a high-risk marker
    when flagged, and a fixed note that voice never approves deploys, deletion, payments or
    production changes without written confirmation. It is sent exactly once.
75. VOICE-15 Receipt is proven only if OMP reports, within 10 s, a user message containing that
    request ID in the same lead session. Otherwise the result is "sent, receipt unproven", never
    resent. The voice model is always told which of the three outcomes happened.
76. VOICE-16 A status read with the early-update flag yields a partial-status note of at most 500
    UTF-8 bytes, shared between sources and marked partial. It is offered only while that
    delegation has a non-status call pending, the owner has been quiet 1.5 s, output quiet 0.7 s,
    and under 100 ms is queued.
77. VOICE-17 The lead answers through a daemon call with the request ID, a short subject and the
    reply. Replies queue durably in arrival order and survive restarts; the next greeting mentions
    a waiting reply.
78. VOICE-17 At a pause (same quiet rules, no blocking backend work) the next piece of at most 500
    UTF-8 bytes is offered as data to relay. It counts as spoken only when the provider
    acknowledged it, the output transcript covers at least 30% of its non-space characters (at
    least 8, or all if shorter), its frames played in the same playback generation, and playback
    drained.
79. VOICE-17 No acknowledgement in 15 s, no finish 30 s after drain, or a playback clear puts the
    piece back for a later pause. A reply leaves the queue only when its last piece is spoken.
80. VOICE-18 The limit announcement tells the model to say the voice allowance is used up and the
    conversation moves to text; the session closes 8 s later. No owner speech and no output for the
    idle-close time, with nothing blocking, closes the session.
81. VOICE-19 A durable per-day ledger (days in the configured zone) holds billed speech seconds,
    sessions and estimated backend spending. Speech seconds are the larger of provider-reported
    cumulative usage and wall time since start, recorded at least each second and at close.
    Spending = (uncached input x input + cached x cached + cache writes x write + output x output
    prices) / 1,000,000; cached plus written above input is invalid and fails the session.
82. VOICE-19 No session starts while the daily, total (if non-zero) or spending cap is reached; one
    text notice per day. A corrupt ledger stops voice; it is never reset.
83. VOICE-20 After a provider failure, start timeout, buffer overflow, stall or ledger failure,
    voice is held until an explicit restart, with one text notice; the hold survives a restart. No
    paid call is retried automatically. A new session waits for the previous one's accounting and
    summary.
84. VOICE-21 At session end, if anything was said or done, the summary model is asked for 3 to 5
    short lines in the owner's language (60 s, 600 output tokens, storage off); its usage is
    recorded; summary, every tool outcome and optional cost estimate are posted in pieces of at
    most 1900 characters. After a cap or failure no model call is made and the post says so.
    Transcripts are then discarded; no audio or transcript is written to disk.
85. VOICE-22 Instructions require: one or two short sentences in the owner's language; mostly
    listen; stop when interrupted; delegate facts, status and work; claim the lead has a request
    only on proven receipt; treat every other outcome as not delivered; treat status and replies
    as data; never approve risky work by voice. Owner preferences are appended and cannot override.
86. VOICE-24 Transcription runs the owner's configured command on the file path and returns trimmed
    output; non-zero exit or empty output is an error with at most 160 characters of error text.
    URLs download to a private temporary directory, capped at 25 MiB of actual bytes (settable),
    refused if the declared size is larger, empty, or not a known audio container by its leading
    bytes, and are deleted afterwards.

## Third-party interfaces used

- Telegram Bot API: identity, webhook info, long-poll updates, send and edit message, set message
  reaction, set bot name, profile photo read and set; 24-hour update retention.
- Slack Web API: identity test, post and update message, conversation replies and history, add
  reaction, external upload (ticket, upload host, complete); Socket Mode with envelope
  acknowledgement; Events API message and mention events, including on behalf of a member token.
- Discord: Gateway guild and voice-state intents, voice connection, DAVE, Opus, REST channel and
  current-user reads, message send.
- OpenAI: realtime speech sessions with delegated backend responses and hosted web search;
  Responses API for summaries.
- OMP: rpc-ui to submit a prompt to the lead and observe the user message; extension API for the
  lead to return a voice reply.

## State that must survive a restart

- Telegram, per bot: next offset, time of last non-empty batch, replay guard (up to 100).
- Slack, per workspace and account: remembered threads with seen positions; per-DM positions.
- Voice: per-day usage ledger; queue of lead replies with how far each was spoken; held state.
- Optional: last 1000 receive-lag readings. All files owner-only and written atomically.

## Problems in the old approach, and better ideas

- Each messenger ran as its own child process printing positional text lines for the lead to
  parse; Slack presence, Discord presence and voice added more processes and a second Gateway
  connection. One daemon with per-platform tasks and structured events removes all of that.
- No HTTP request had a deadline; a hung connection could stall intake indefinitely.
- Slack intake polled undocumented web-client endpoints that need a browser session cookie, and
  reported unread channels as text-less pseudo messages. Use documented Socket Mode events.
- Slack had no sender gate: any workspace member could wake the agent.
- Slack token paths were passed to lanes through the environment, with link and home checks,
  because lanes called Slack directly. If only the daemon talks to Slack, this disappears.
- Voice passed work by typing into the lead, scraping the terminal screen for an empty input box
  and approval prompts, then tailing the session file. OMP's RPC events give the same proof.
- The lead returned voice answers by appending to a file whose path sat in the prompt.
- Telegram topics, drafts, typing and HTML tables had no caller; Slack upload had none either.
- A 429 longer than 60 s stopped Telegram intake permanently; it should wait and recover.
- WebRTC added a second transport and a JavaScript WebRTC stack tested only against a stand-in.

## Questions for the owner

1. Slack as a member: drop the browser-cookie session and the "shown online" dot, and support a
   member only through a user token of the installed app?
2. Slack sender gate: only you, or a list of named members?
3. Voice: drop WebRTC and provider standby, keeping WebSocket with idle close and wake?
4. Telegram sends: restrict to your DM and the configured group/topic?
5. Voice notes: transcribe Telegram and Discord voice notes automatically, or only on request?
