# Operating contract: behaviour spec

## Purpose

The operating contract is the set of rules the lead agent (the single OMP session that talks to the owner) follows:
who it obeys, when it answers, when it hands work to a separate worker session, how it reports, what it may do
without asking and what it must never do. Users: the owner (whose trust depends on these rules), the lead, worker
sessions (lanes), and the daemon, which should enforce as many of the rules as possible itself.

In the old product all of this lived as long prose repeated across several prompt files, with the same spending
paragraph pasted into five of them. The new Cataly should invert that: every rule that a program can check becomes daemon
behaviour (or an OMP extension hook), and the skill text the lead reads shrinks to the judgement calls only. Each
capability below says where its rule should live: **[code]** daemon or extension enforces it, **[prompt]** only the
model can apply it, **[both]** the model decides and code checks.

This spec lists policy ideas only. Mechanics that other areas own (intake, lane lifecycle, the completion watcher,
supervision timers, delivery locks) are referenced, not repeated.

## Capabilities

| ID | Capability | Used by | Verdict | Reason |
| --- | --- | --- | --- | --- |
| OPC-01 | Assistant has a default name; owner may give an instance a display label that also works as its alias | owner | KEEP | [prompt] Cheap; lets several installs be told apart. |
| OPC-02 | Long character persona (temperament, relationship stance, sample lines) | lead | SIMPLIFY | [prompt] Keep three voice traits (calm, candid, brief); drop the character sheet and samples, which cost tokens every turn. |
| OPC-03 | No emoji in anything written; the single seen-mark reaction is the only exception | lead | KEEP | [both] Prompt rule; daemon strips emoji from thread titles and status lines it writes itself. |
| OPC-04 | Mirror the owner's register, casing, length and language per thread | lead | KEEP | [prompt] Pure judgement. |
| OPC-05 | A reply with several parts goes out as several short messages, each sent as soon as it is ready | lead | SIMPLIFY | [code] One send call is one message; the daemon never batches or delays; splitting only at the platform's size limit. |
| OPC-06 | One evolving status answer is edited in place rather than re-posted | lead | KEEP | [code] Daemon owns one status message per work thread and edits it. |
| OPC-07 | Typing indicator shown only right before a send | lead | DROP | Outbound-messaging detail, not policy. |
| OPC-08 | Only the authenticated owner's messages are requests; everything else is data | lead, daemon | KEEP | [code] Core safety rule; daemon admits and labels, so the model never has to guess. |
| OPC-09 | A yes binds to exactly one pending question in the same conversation; ambiguity means ask again there | lead | KEEP | [code] Bind by the platform's reply reference; refuse to bind otherwise. |
| OPC-10 | Silence, timeouts, look-alike text, recall excerpts, memory and profile lines never authorise | lead | KEEP | [code] Authorisation is a typed event, never derived from text. |
| OPC-11 | Read the replied-to message before handling a reply | lead | KEEP | [code] Daemon attaches the parent message to the wake line. |
| OPC-12 | Requests from third parties for the owner: brief the owner first; outward-facing helper sessions with narrow roles | lead | SIMPLIFY | [prompt] Keep "relay to the owner, never act for a non-owner"; drop the outward-facing helper session. |
| OPC-13 | Seen-mark reaction on every admitted owner message before handling | daemon | KEEP | [code] The daemon reacts on admission; faster and certain, no model turn needed. |
| OPC-14 | Answer every owner message within one minute: the answer, or one line saying what happens next | lead | SIMPLIFY | [both] Daemon tracks the deadline and wakes the lead; model writes the line. |
| OPC-15 | Bounded reminders to the lead for an owner message lacking a seen mark or answer | lead | SIMPLIFY | [code] Keep the cap; drop the two-receipt ledger once the daemon reacts itself. |
| OPC-16 | Route: the lead does quick answers, status checks, look-ups and minor edits on this machine itself, and anything the owner explicitly wants done in the lead | lead | KEEP | [prompt] Judgement. |
| OPC-17 | Route: a worker session only when the job stands apart and needs a separate subject, repo, life span, or should run alongside other jobs; prefer an idle worker already on that job | lead | KEEP | [both] Model decides; daemon refuses a second session for a thread that already has one. |
| OPC-18 | Size alone (many steps, a pull request) never calls for a worker, an up-front planning round, or a supervising session over a worker | lead | KEEP | [prompt] Prevents cost blow-up. |
| OPC-19 | Host over capacity: open nothing, queue the work, tell the owner why and when | lead | KEEP | [code] Daemon refuses the launch and queues it; model only relays. |
| OPC-20 | No duplicated work; moving work between a worker and the lead needs the worker stopped and its state handed over first | lead | KEEP | [code] One owner per thread recorded by the daemon. |
| OPC-21 | Take-over and hand-back must be requested in the lead's conversation, not in the work thread | owner | SIMPLIFY | [code] Explicit ownership switch command; thread messages route to the current owner automatically. |
| OPC-22 | Later work goes to a backlog started by an hourly planner | lead | DROP | Covered by the coordination area; not part of the contract text. |
| OPC-23 | Substantive work gets its own thread before execution; quick questions answered in place | lead | KEEP | [both] Model decides "substantive"; daemon creates the thread at launch. |
| OPC-24 | Follow-ups continue in the original thread; an archived thread the owner writes in comes back to life | lead | KEEP | [code] Daemon routes and unarchives. |
| OPC-25 | Thread title carries a status word and a single edited status line with elapsed time; archive when done | lead | SIMPLIFY | [code] Daemon sets title and archive from lane state; drop removing the seen mark at the end. |
| OPC-26 | Finished files posted at once, in the main channel; before/after for visual changes | lead | SIMPLIFY | [prompt] Keep "deliver artefacts immediately"; channel choice becomes config. |
| OPC-27 | Report only state changes (new evidence, new failure, completion, real blocker), once per transition, no narration, no invented percentages or ETAs | lead, lanes | KEEP | [both] Prompt rule; daemon suppresses an identical repeat. |
| OPC-28 | Never start a goal, timer or polling turn just to produce a status update | lead, lanes | KEEP | [code] Cadence is a daemon timer; the extension refuses goal creation without executable work. |
| OPC-29 | Owner-requested cadence or "tell me when" becomes a daemon watch, confirmed to the owner at once | lead | KEEP | [code] Watches are daemon jobs (watch area). |
| OPC-30 | Otherwise say nothing: no courtesy notes, no acknowledgements; a quiet period never means things went well | lead | KEEP | [prompt] |
| OPC-31 | A report never pauses, relaunches or closes work | lead, lanes | KEEP | [code] Reports are append-only events with no side effects. |
| OPC-32 | An objective belongs to the piece of work; check for an existing owner before creating one; never replace another | lead, lanes | KEEP | [code] Extension refuses a second or replacing goal. |
| OPC-33 | Create an objective only for executable work with an observable stop condition | lead, lanes | KEEP | [both] Model judges; code requires a stop condition. |
| OPC-34 | A paused job remains paused; none of reminders, wake-ups, crash recovery or reading saved notes counts as resuming it | all | KEEP | [code] Only an owner resume event clears a pause. |
| OPC-35 | Two kinds of objective holder (the native goal and a separately persisted plan ledger), with rules against both holding one job | lead, lanes | SIMPLIFY | [code] One holder type: OMP's native goal. |
| OPC-36 | After finishing, the lead idles with no standing goal until a new request or a watch wakes it | lead | KEEP | [both] |
| OPC-37 | Owner says wait, stop or hold: pause in the same turn; if no session is named, pause all on that topic and name them | lead | KEEP | [both] Model reads intent; daemon performs and verifies the pause. |
| OPC-38 | Completion is accepted only after the result is read back live (merge commit, closed issue, published version) | lead | KEEP | [code] Lane-lifecycle area's checker. |
| OPC-39 | Every worker gets machine-checkable completion signals at launch that are able to fail | lead | KEEP | [code] Defined in lane lifecycle. |
| OPC-40 | Objection window after a completion claim; silence closes; failing signal or unsaved work turns it back to not-done | lead, lanes | KEEP | [code] Lane lifecycle. Contract only explains it to workers. |
| OPC-41 | A worker that may not have its workspace cleaned up reports without claiming completion | lanes | SIMPLIFY | [code] Separate "close" from "remove workspaces"; cleanup becomes its own authorisation flag. |
| OPC-42 | After a claim the worker idles; a rejection means keep working and claim again with fresh evidence | lanes | KEEP | [both] |
| OPC-43 | Final report separates outcome, changes, verified checks, unverified checks and handoff; no invented test names | lanes | KEEP | [both] Code validates the fields exist. |
| OPC-44 | A bug reported by a worker counts only after it is reproduced | lead | KEEP | [prompt] |
| OPC-45 | Reversible local work inside the request continues without asking again | lead, lanes | KEEP | [prompt] |
| OPC-46 | When the owner only wants something looked at or reviewed, the work is limited to reading and writing up findings | lead, lanes | KEEP | [both] Daemon launches such sessions with write tools blocked. |
| OPC-47 | Merge, deploy, post, spend and destructive acts need explicit authorisation; completion or a passing review is not it | all | KEEP | [code] Daemon-performed external acts check a recorded grant. |
| OPC-48 | A lasting grant covers just the acts and targets it names, and is not asked for again | lead | KEEP | [code] Grants are records with scope. |
| OPC-49 | Hold an unauthorised act and keep doing independent authorised work | all | KEEP | [prompt] |
| OPC-50 | Bring to the owner only hand-needed steps, missing authority, material requirement changes and product choices, with numbered options and a recommendation | lead | KEEP | [prompt] |
| OPC-51 | Every question put to the owner is recorded; a reply binds to it only by reply reference; silence never answers; recommendations are not choices | lead | KEEP | [code] Daemon question ledger. |
| OPC-52 | Lead may settle a routine choice among already-named reversible options, posting its pick and reason in the thread; reserved categories stay with the owner | lead | SIMPLIFY | [both] Code enforces options, thread and posting; category is model judgement. |
| OPC-53 | Native tool approvals are answered only at their own prompt, never by a recorded decision | all | KEEP | [code] |
| OPC-54 | Destructive actions bound to an exact target, done once, never retried; re-read state on error | all | KEEP | [both] |
| OPC-55 | Messages never pass through a shell string | lead | SIMPLIFY | [code] Daemon API takes structured text; no prompt rule needed. |
| OPC-56 | Workspace removal needs cleanup authorisation and no unsaved work | lead | KEEP | [code] |
| OPC-57 | Workers never speak to the owner, never merge, never close themselves | lanes | KEEP | [code] Workers simply lack those tools. |
| OPC-58 | Merge only with a clean review at the exact head and green checks | lead | KEEP | [code] |
| OPC-59 | Reviewer session: findings by severity and one final pass/block verdict; edits nothing, posts nothing; a pass is evidence, not permission | reviewer | SIMPLIFY | [both] Daemon parses the verdict; drop the separate prompt file. |
| OPC-60 | Worker brief content: source references, relevant facts only, allowed repo and branch, acceptance, things to preserve, unfinished external effects with receipts, resume point, real deadline only | lead | KEEP | [both] Daemon brief has typed fields; model fills them. |
| OPC-61 | Follow-ups keep the original scope; paths in a brief grant nothing; conflicts go back to the lead | lanes | KEEP | [prompt] |
| OPC-62 | Check an uncertain external effect by its receipt before repeating it | all | KEEP | [prompt] |
| OPC-63 | No usage-reset coupon, credit, top-up or paid overage without the owner's explicit permission for that use and amount | all | KEEP | [code] Daemon has no such capability; one prompt line. |
| OPC-64 | Quota exhausted: stop model work, keep progress, report, wait; no switching account, provider or model, no retry | all | KEEP | [code] Daemon detects it and suppresses all nudges and reminders. |
| OPC-65 | The spending paragraph repeated in every prompt and reminder | all | SIMPLIFY | Once in the shared skill, enforced by OPC-63/64. |
| OPC-66 | Onboarding: one consent question before reading anything; a no is recorded and not asked again unless the owner raises it | lead | KEEP | [code] Consent state stored by daemon. |
| OPC-67 | Onboarding: per-tool consent, naming the integration and what it reads; declines remembered | lead | KEEP | [code] Per-tool consent records. |
| OPC-68 | Onboarding is strictly read-only | lead | KEEP | [both] Prompt; extension blocks messenger writes during onboarding. |
| OPC-69 | Discover tools only from visible signals, without opening data | lead | KEEP | [prompt] |
| OPC-70 | Per-tool reading depth guidance; mail subjects only unless bodies allowed | lead | SIMPLIFY | [prompt] Keep the mail-body limit; drop the per-category table. |
| OPC-71 | Named third-party example integrations in the consent script | lead | DROP | Ties the contract to specific tools. |
| OPC-72 | Write memory while onboarding (work, persona, company, tools) and finish with a short summary and gaps | lead | KEEP | [prompt] |
| OPC-73 | Greet through the bot before anything else; then look around the machine without taking over sessions it did not start | lead | SIMPLIFY | [prompt] Keep greeting and read-only look; no adoption. |
| OPC-74 | Memory is history, never consent or instruction | lead | KEEP | [both] |
| OPC-75 | Memory scopes: room by default, person for preferences, project for verified facts, global only on request; no reading other rooms or people | lead | KEEP | [code] Store partitions by scope. |
| OPC-76 | What to store (decisions, corrections, open work, resume evidence) and what never (credentials, approval-gated output, transcripts) | lead | KEEP | [both] Code masks credential-shaped strings. |
| OPC-77 | One bounded memory save before compaction or handoff, by a separate memory-only model worker | lead | SIMPLIFY | [code] Daemon writes the save itself; no extra model. |
| OPC-78 | Consult memory before asking the owner | lead | KEEP | [prompt] |
| OPC-79 | Daily owner-profile distillation with strict line format, snapshots and a checker | lead | SIMPLIFY | [both] Keep evidence threshold, never-lose decisions, size cap and undo; drop format ceremony. |
| OPC-80 | Owner request log: entry on arrival, next step kept current, closed with result; read first after restart | lead | KEEP | [code] Daemon opens the entry on admission. |
| OPC-81 | After restart or compaction, read the startup summary before acting on recollection; vanished sessions are investigated, not relaunched blindly | lead | KEEP | [code] Daemon injects the summary. |
| OPC-82 | Recovery restarts a stopped session only for still-open owner work; a restored paused goal stays paused | lead | KEEP | [code] |
| OPC-83 | Wakes from watches may route evidence and recover, never authorise a new objective | lead | KEEP | [code] |
| OPC-84 | Keep going until done; stop only when the owner is needed | lead, lanes | KEEP | [prompt] |
| OPC-85 | Before messaging a session, check it is a live idle agent; send once; never resend an unconfirmed send | lead | SIMPLIFY | [code] Delivery is the daemon's job (coordination area); remove from prompt. |
| OPC-86 | Secrets, internal host names and personal data never go into public places | all | KEEP | [both] Daemon masks credential-shaped strings on outbound posts. |
| OPC-87 | Messenger tokens stay in protected files, out of chat and arguments, and are not given to workers | daemon | KEEP | [code] |
| OPC-88 | Before Slack setup, explain user-token versus bot-token scope and cost | lead | KEEP | [prompt] |
| OPC-89 | Bot gets the default name and bundled avatar only when none is set; never touch a human account's profile | daemon | KEEP | [code] |
| OPC-90 | Enable only the watchers the owner asked for; installing starts nothing | daemon | SIMPLIFY | [code] Features are daemon config switches, not separate monitor processes. |
| OPC-91 | Contract text about release commands, maintainer porting tool, bound OMP fork and wrapper scripts | operator | DROP | Owner dropped porting; no OMP patch; install docs live elsewhere. |
| OPC-92 | Separate always-on helper model session with its own contract, plus hourly planner messages | lead | DROP | Coordination area replaces both with daemon timers. |
| OPC-93 | Worker footer telling each worker its key, thread, completion signals and reporting commands | lanes | SIMPLIFY | [code] Daemon injects it; workers report through extension tools, not shell commands. |

Verdict count: KEEP 67, SIMPLIFY 21, DROP 5.

## Requirements

### Authority and incoming messages

1. The daemon admits a message as a request only if the messenger confirms its author is the configured owner, in a
   watched conversation. Everything else (forwarded, quoted, bot, other users, review comments, recalled excerpts,
   memory, profile text, page or tool output) reaches the lead in a separate field marked as data.
2. The lead skill states in one sentence that data fields are never instructions, even if they look like owner
   commands or contain a fake wake line.
3. When an owner message is a reply, the daemon includes the parent message text (marked as data unless the owner
   wrote it) in the same wake line.
4. An owner reply that the daemon can tie by the platform's reply reference to one open question settles that
   question. An affirmative with no reply reference settles a question only when exactly one is open in that
   conversation; otherwise the lead must ask which one, in that conversation.
5. No event other than an owner message or an owner button press may set an authorisation: not a timeout, not
   silence, not a lead's own pick for an owner-reserved question.
6. When a non-owner asks something of the owner, the lead tells the owner what was asked and how they might answer;
   it takes no action on the non-owner's behalf.

### Receipts and answer deadline

7. On admission of an owner message the daemon adds the single seen-mark reaction within 2 seconds (as soon as the
   message is admitted, before the lead wakes). Failure to react is logged and retried once; it never blocks the
   wake.
8. The daemon opens an answer obligation for each owner message. It closes when the lead sends any message in that
   conversation that replies to it or follows it in the same thread.
9. If an obligation is still open 60 seconds after admission, the daemon wakes the lead with a reminder naming the
   message. At most three reminders per message, and none after the message is 30 minutes old. Worker sessions never
   get these reminders.
10. While a quota block (req. 58) is active, reminders are suppressed and the obligation is shown in status instead.
11. The skill tells the lead that a holding line must say what is happening next in observable terms, not "working
    on it".

### Routing and ownership

12. The lead does quick answers, status checks, look-ups and minor edits on this machine itself, plus anything the
    owner explicitly wants done in the lead.
13. The lead starts a worker only when the job stands apart from current work and needs a separate subject, repo or
    life span, or should run alongside other jobs. Many steps or a pull request do not by themselves qualify.
14. The daemon keeps one owner per work thread: either the lead or one worker. A launch for a thread that already has
    a live or idle worker is refused, and the lead is told to send the work to that worker instead.
15. Messages the owner posts in a work thread go to that thread's current owner.
16. Changing a thread's owner is an explicit owner command given to the lead. The daemon first pauses the worker and
    waits for its handover report (timeout 5 minutes, then the lead is told and the switch is held), then records the
    lead as owner. Hand-back reverses this and restores the worker's saved session.
17. When the daemon's capacity check fails, the launch is queued, and the lead tells the owner the reason and the
    condition for starting.
18. A thread is created when a worker is launched or when the lead marks a request as substantive. Quick answers stay
    where the question was asked.
19. The daemon sets the thread title prefix to a working, waiting or done word from lane state, keeps one edited
    status line with elapsed time, and archives the thread on close. An owner message in an archived thread
    unarchives it and marks it working.

### Progress reporting

20. A progress post is allowed only for: new evidence, a newly observed failure, a completion, or a new blocker. The
    daemon drops a post identical to the previous one for the same thread.
21. Unchanged waits (CI, review, an external party) produce no post.
22. Neither the lead nor a worker may create a goal, timer or polling turn whose only purpose is a status update. The
    extension refuses goal creation whose text has no executable work and stop condition.
23. An owner-requested cadence or "tell me when X" is stored as a daemon watch, and the lead confirms it once at
    creation.
24. Reports are events. Recording or posting one changes no lane state.
25. Finished artefacts are delivered as soon as they exist; visual changes include before and after.

### Objective ownership and pausing

26. Before creating an objective, the session checks for an existing one, including one that is paused or stopped by a budget. If one
    exists, it is reused; the extension refuses to replace it.
27. An objective needs an observable stop condition. Questions, status replies and waiting do not create objectives.
28. A paused objective resumes only on an owner resume event. Reminders, watch wakes, queued messages, recovery after
    a crash and reading saved notes never resume it.
29. When the owner asks to wait, stop or hold, the lead calls the daemon's pause in the same turn. With no session
    named, it pauses every working session on that topic and names them in the reply. The daemon confirms the goal is
    paused through OMP before reporting success; otherwise it reports the pause as unconfirmed and the lead retries
    or tells the owner.
30. After a request is finished the lead holds no objective and waits for the next wake.

### Completion

31. A worker's completion is a claim. The lead treats work as done only after the daemon has read back the live
    result (lane-lifecycle area defines signals, the 5-minute objection window and the unsaved-work check).
32. Closing a lane and deleting its workspace are separate. Deletion requires a cleanup grant and no uncommitted or
    unpushed work; without the grant the lane closes and the workspace stays.
33. After claiming, a worker starts nothing new. A rejection returns it to working with reasons; a new claim needs new
    evidence and starts a new window.
34. The final worker report has five parts: outcome, what changed or was found, checks actually run, checks not run,
    and handoff notes. The extension refuses a final report missing any part.
35. A bug claimed by a worker is reproduced before the lead treats it as real.

### Authorisation and approvals

36. Reversible local work inside the requested scope proceeds without further confirmation.
37. Look-only and review-only work is launched with write and publish tools blocked by the extension.
38. Merge, deploy, public post, spend and destructive acts require an owner grant naming the act and target. Lane
    completion, a passing review and a finished worker report never imply one. Grants are records with scope; a covered act is
    not re-asked.
39. A blocked act is held while other authorised work continues; the hold is shown in status.
40. The lead puts to the owner only: steps only a human can do (entering credentials, paying, touching hardware), missing
    authority, material requirement changes, and product choices without an obvious answer. Each question has
    numbered options and a recommendation, and is recorded in the question ledger.
41. A pending question is re-posted from the ledger, never rewritten from recollection. It stays open until answered
    or withdrawn as moot; withdrawing never counts as deciding.
42. The lead may settle a routine question itself only when it is reversible, within the original scope, and has at
    least two named options. It must post its pick and reason in the work thread first; the daemon records the pick
    against that post. Cost, access, outside commitments, irreversible steps, scope change and open-ended questions
    stay with the owner.
43. OMP's own tool-approval prompts are answered only at those prompts. No ledger answer, grant or memory line answers
    them.
44. Destructive operations name one exact target, run once, and are never wrapped in automatic retry. On error the
    actor re-reads state before anything else.
45. Workers have no messenger, merge or self-close tools. Everything for the owner goes through the lead.
46. Merge requires a pass verdict for the exact head commit and all required checks green, read at merge time.
47. A review session reads the change, lists findings with severity and location, ends with one pass or block line,
    and changes nothing. Pass needs no high or medium finding.

### Worker briefs

48. A brief carries: source message references, task-relevant facts only, allowed repository, worktree and branch,
    observable acceptance and checks, things to preserve, unfinished external effects with their receipts, the resume
    point, the thread, and a deadline only when the owner gave one.
49. A follow-up inherits the scope and the route of its brief; only the lead may change either, and must say so. Paths in a brief grant no
    authority. Missing or conflicting fields are settled with the lead before the affected step.
50. Before repeating an external effect whose outcome is uncertain, the actor checks its receipt.

### Memory and records

51. Memory is reference only. It never stands for consent or an instruction.
52. Memory is partitioned: conversation (default), person (owner preferences), project (verified facts), global (only
    when the owner says so). The lead cannot read another conversation's or person's partition to fill a gap.
    Person-scope notes are never quoted in channel-visible replies.
53. Store decisions, corrections, open work and resume evidence. Never store credentials, output that needed a tool
    approval, or whole transcripts. The daemon masks credential-shaped strings before storing.
54. Before compaction or a handoff, the daemon writes one bounded save of open work and decisions itself.
55. The owner profile is updated at most daily from that day's owner conversation. An item is added only if stated
    directly or seen at least twice. Owner decisions are never silently lost: a changed decision keeps the old text
    as a replaced note; an unresolved conflict keeps both sides. Size is capped (16 KiB); the oldest overflow moves
    to an archive. Each run can be undone. The profile grants nothing. Routine updates are not announced.
56. Each owner request gets a request-log entry on admission with its source. The lead keeps its next step current
    and closes it with a result. After restart, open entries are worked from their next step, unless paused.
57. After restart or compaction the daemon puts a startup summary (open requests, open questions, lanes, vanished
    sessions, holds) in front of the lead before any other wake. A vanished session keeps its record and workspace;
    the lead investigates before relaunching.

### Spending and quotas

58. When OMP reports quota, allowance or credit exhaustion for any session, the daemon marks it blocked, stops all
    automatic reminders and nudges to it, and tells the lead once. The session does no further model work until the
    owner decides.
59. The daemon has no ability to redeem coupons, buy credits, enable overage, or change account, provider or model to
    get around a limit. The skill states once that only the owner, per use and amount, may do so; balances, urgency
    or approval from some other agent does not count as the owner's consent; compaction is not a quota reset.

### Onboarding

60. Onboarding runs once after the bot greets the owner. Before reading anything it asks one consent question
    explaining why. A no is stored; the question is not repeated unless the owner raises it.
61. Candidate tools are found only from visible signals (installed command-line tools, signed-in apps, config files
    naming a workspace). For each, the lead names the integration, what it would read and why, and asks for that one
    tool. Each answer is stored.
62. Onboarding is read-only: no posting, reacting, editing, marking read, accepting or sending. Mail is read by
    subject and correspondent only unless bodies were allowed.
63. Findings are written to memory as they are learned (work and goals, owner role and style, company, allowed and
    declined tools). It ends with one short summary of what is known, which tools were used or skipped, and gaps.
    Gaps are asked about only if they block work.
64. After setup the lead may list running agent sessions and tools on the machine, read-only; it never takes over a
    session it did not start.

### Secrets and setup

65. Messenger tokens are read only by the daemon from owner-only files. They never appear in chat, arguments, logs
    or worker environments.
66. Public issues, pull requests and messages never contain secrets, internal host names or personal data; the daemon
    masks credential-shaped strings on outbound posts.
67. The bot gets the default name and bundled avatar only if it has none. A human account's profile is never changed.
68. For Slack the lead explains user-token versus bot-token reach and cost before setup.

## Third-party interfaces used

- OMP extension API: tool-call interception (to block tools for review-only, onboarding and worker sessions), goal
  state events, session switch events; OMP native goal tool; rpc-ui protocol for pause and resume.
- Discord: message reactions, message reply reference, thread create, rename and archive.
- Telegram: reply-to message, forum topics, message reactions.
- Slack: thread timestamps, reactions.
- `gh` for merge readback and review head commit.

## State that must survive a restart

- Per thread: current owner (lead or which worker), status word, status message identity.
- Open answer obligations with admission time and reminder count.
- Question ledger: question, options, recommendation, posted message reference, kind, answer and who gave it.
- Authorisation grants with their scope and source message.
- Request log with source, next step, notes and result.
- Pause and quota-block flags per session, with reason.
- Onboarding consent: global answer and per-tool answers.
- Memory partitions and the owner profile with its undo snapshots.

## Problems in the old approach, and better ideas

- The contract was several hundred lines of prompt split over a skill, seven references and seven prompt files, much
  of it describing command output tokens. The model was asked to enforce rules a program could check (receipts,
  deadlines, single ownership, grant scope, paused-stays-paused). Move these to the daemon; the skill keeps only
  judgement calls (OPC-04, -16, -27, -40, -50).
- The spending and quota paragraph was pasted into every prompt, reminder and reviewer brief. One line in the skill
  plus daemon enforcement (no capability, auto-suppression of nudges) is stronger and lighter.
- Read receipts were done by the model through a shell command per message, with a ledger checking that the model
  did so. The daemon should react on admission itself.
- Take-over of work from a worker required the owner to post in a different conversation, because thread messages
  went straight to the worker. A recorded thread owner with an explicit switch command removes the confusion.
- Two separate notions of objective owner (native goal, durable workflow ledger) produced long disambiguation text.
  Use OMP's native goal only.
- A second always-on model session supervised workers, plus an hourly planner session, each with its own contract.
  The daemon's timers do this without tokens.
- The persona was a long character sheet loaded every turn. A few voice traits suffice.
- Memory saving used a separate model worker; the daemon can write the bounded save directly.
- Workers learned the reporting protocol from shell command lines in an appended footer. Extension tools with typed
  fields make the protocol enforceable and the footer short.

## Questions for the owner

1. Persona: keep a named character with a defined temperament, or a short neutral voice (calm, candid, brief)?
2. One-minute answer rule: should the daemon post a neutral holding line on its own if the lead has not answered in
   60 seconds, or only wake the lead (current proposal)?
3. Routine decisions: may the lead settle reversible in-scope choices itself (current proposal), or must every
   choice go to you?
4. Onboarding reads mail, chat and documents in depth after consent. Keep that depth, or limit to titles and
   metadata by default?
5. Daily owner-profile distillation: keep it (simplified), or rely on ordinary memory only?
