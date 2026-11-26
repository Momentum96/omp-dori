# Remote workers

Status: proposal for owner approval, 2026-11-27. Nothing here is built. Judged against
`docs/principles.md` (thin, light, fast, stable) and the code on `main` at `7cc4ddbe`. OpenSSH and
Tailscale claims cite their public documentation (Sources at the end). Estimates are marked
**[est.]**, unverified inferences **[inf.]**.

## 1. Recommended design

- **One hub, one bot.** The hub machine keeps the relays, the lead, the store and every job. A
  **worker** is another machine (or another OS user) that runs only lanes. It holds no relay token, no
  Cataly secret and no store; it uses its own OMP and its own provider login.
- **Same binary.** A worker runs the released `cataly` in worker mode (`cataly worker serve`). No
  service on the worker: its sshd starts worker mode for each hub connection, so a worker reboot needs
  nothing but sshd.
- **OpenSSH is the only transport and the security boundary.** One ed25519 key per enrolment, in the
  worker's `authorized_keys` as `restrict,command="… cataly worker serve --hub <id>"`. The worker's host
  key is pinned in Cataly's own `known_hosts`. Cataly opens no port.
- **Tailscale is the network, never the SSH layer.** A worker on the owner's tailnet is addressed by its
  MagicDNS name or tailnet IP. Tailscale SSH is not used (2.3).
- **Option B for drops.** OMP ends with the connection; the lane comes back from its session file on the
  next connection through the existing hub-restart path (decision 9). No resident process per lane.
- **Bridge over the worker-mode stream.** The worker-mode process opens a local Unix socket for the
  bridge and carries each bridge connection as a channel of the SSH stream. No `-R` forwarding. The
  extension file is the one the worker's own `cataly` installs. `cataly.ts` does not change.
- **Placement from evidence.** `lane.open` gains `worker`. The lead uses it only when the owner names the
  machine or a platform only it offers, or the repository is listed for that worker. Repositories match
  by Git origin; the worker uses its own clone and a worktree at the requested commit; uncommitted hub
  edits are not carried, and the open says so.
- **Checks where the facts are.** `command` and `file` run on the worker; `pr_merged`, `issue_closed`,
  `web` and `pr_policy` run on the hub.

## 2. Decisions and reasons

### 2.1 One process model, one runtime

- Hub side: one `ssh` client child per worker in use, closed after 5 minutes with no remote lane and no
  pending operation; its memory is measured in phase 1. Worker side: one `cataly worker serve` per hub
  connection, plus the OMP children of its lanes. Nothing else.
- Remote lanes are **headless** only. Tern lanes need a terminal on the hub's screen; a remote tern lane
  is refused by name.
- The `Headless` runtime stays one implementation. What changes is where its child lives: `command::attach`
  becomes `Place::attach`, where `Place` is `Local` or `Worker(name)`. A remote child gives the same
  stdin, stdout and close as a local one, so `omp::rpc`, the session actor, dialogs, the spend guard, the
  model barrier and delivery receipts are untouched (**light**: one implementation per concern).
- The same `Place` routes the other per-machine work: bounded subprocesses (`git`, `omp --version`, the
  probe, `omp session lease`, `command` checks) and bounded file reads (transcripts, `file` checks).
  Parsers stay on the hub; the worker moves bytes.

### 2.2 Transport: OpenSSH

Why OpenSSH and not an SSH library in the hub: no crypto or SSH dependency, the owner's sshd stays the
one audited server, and `known_hosts`, keys and forced commands are standard, documented behaviour
(**thin**, **light**).

- **Hub key.** `ssh-keygen -t ed25519 -N ''` writes `<config>/secrets/worker-<name>` (0600). One key per
  worker, so `worker remove` revokes exactly one machine.
- **Worker line**, written by `worker init`:
  `restrict,from="<hub tailnet IPs>",command="/home/dev/.local/bin/cataly worker serve --hub 3f2a9c" ssh-ed25519 AAAA… cataly-hub-3f2a9c`.
  `command=` ignores what the client asks for and puts it in `SSH_ORIGINAL_COMMAND`; `restrict` disables
  port, agent and X11 forwarding, PTY allocation and `~/.ssh/rc`, and future restrictions too; `from=` is
  added only with `--tailscale` (2.3) [sshd(8)]. The path is the stable command link, so a worker
  `cataly update` does not break the line.
- **Client.** The hub runs `ssh -F none -T -i <key> -p <port>` with `IdentitiesOnly=yes`,
  `IdentityAgent=none`, `UserKnownHostsFile=<config>/known_hosts`, `GlobalKnownHostsFile=/dev/null`,
  `StrictHostKeyChecking=yes`, `CheckHostIP=no`, `UpdateHostKeys=no`, `BatchMode=yes`,
  `ClearAllForwardings=yes`, `ConnectTimeout=10`, `ServerAliveInterval=15`, `ServerAliveCountMax=3`
  [ssh_config(5)]. `-F none` reads no config file [ssh(1)], so the owner's `~/.ssh/config` cannot add
  forwardings, control sockets or proxy commands.
- **Marker command.** The client asks for the command `cataly-worker-v1`, which does not exist. A server
  that applies the forced command ignores it; a server that runs it instead (Tailscale SSH, or a line
  that lost its `command=`) fails with "not found", and the hub refuses that worker. This catches a
  missing boundary even when the key is accepted (**stable**: fail closed).
- **Host key pin.** Step 2 of enrolment fetches the ed25519 host key with `ssh-keyscan`, compares its
  fingerprint with the one `worker init` printed, and writes the pin. A non-22 port is pinned as
  `[host]:port`, OpenSSH's own form [sshd(8), SSH_KNOWN_HOSTS FILE FORMAT]. A changed key is refused by
  `StrictHostKeyChecking=yes` and never accepted automatically.
- **Stream.** One SSH session per worker carries frames: `u32 length | u32 channel | u8 kind | payload`,
  at most 1 MiB + 9 bytes. Kinds: `open`, `data`, `credit`, `eof`, `close`, `ping`. Each channel has
  1 MiB of credit per direction, so one slow lane cannot stall the others or grow memory. Both ends ping
  every 15 s; 45 s without a frame closes the stream (the bridge's own heartbeat rule). Binary framing
  because the payloads are already JSON lines up to 1 MiB; re-escaping them would double the work.
- **Channel 0: worker protocol v1.** Requests and replies in the control protocol's JSON shape
  (`{v, id, method, params}`), reusing `proto`. Methods:

| Method | Does | Bound |
| --- | --- | --- |
| `hello` | Protocols (worker, bridge), version, OS, arch, root path, `omp` and `git` versions, bridge-link check | 10 s |
| `status` | Load, free memory and disk, through the same `sys::host` readers | 5 s |
| `exec` | One bounded subprocess (argv, cwd, env names, timeout, cap), as `sys::command` | per call |
| `spawn` / `signal` | Start the worker's own OMP (argv[0] must be `omp`, replaced by the configured path) on a new channel; TERM or KILL only its own children | start budget |
| `read` / `stat` / `write` | Bounded file access, confined to the worker root | 8 MiB, 10 s |
| `forget` | Remove this hub's line from `authorized_keys` | 5 s |

### 2.3 Network: Tailscale

Tailscale is optional and only carries packets. Verified against Tailscale's public docs:

- MagicDNS gives every device a name `<machine>.<tailnet>.ts.net`, and search domains let the bare
  machine name resolve too [MagicDNS]. Tailnet IPv4 addresses come from `100.64.0.0/10` [100.x].
  Traditional SSH "on top of Tailscale as the network layer" is a documented setup that exposes no
  server to the internet [Tailscale SSH].
- **Tailscale SSH cannot carry the boundary.** With it on, "Tailscale takes over port 22 for SSH
  connections incoming from the Tailscale network" and authenticates by tailnet identity, not by your
  keys; `sshd_config` and `authorized_keys` are left alone and only non-tailnet connections still reach
  OpenSSH. Tailscale lists "machines where you use `authorized_keys` to limit what commands remote users
  can run" as a poor fit [Tailscale SSH]. So a forced command does not apply on port 22 of a tailnet IP
  when Tailscale SSH is on.
- **Then:** run OpenSSH on another port bound to the tailnet address (`ListenAddress 100.x.y.z:2222`),
  enrol `dev@linux-box:2222`, and the pin is `[100.x.y.z]:2222`. On Linux, sshd must start after
  tailscaled holds the address, or it cannot bind it [inf.]; `worker init` prints the drop-in to order
  it. Tailscale SSH serves only Linux and macOS devices running the open-source `tailscaled` [Tailscale
  SSH], so a Mac on the App Store client keeps port 22 for OpenSSH.
- A Tailscale SSH server also has its own host key, distributed by the control plane [Tailscale SSH], so
  its fingerprint never matches the one `worker init` printed; the marker command (2.2) catches it too.
- **`--tailscale` (convenience, light).** It needs the `tailscale` CLI and is refused without it:
  `worker add dev@linux-box --tailscale` resolves the name with `tailscale ip -4 linux-box` and stores
  the tailnet IP as the destination, with the name for display. The hub's own `tailscale ip` output
  becomes the line's `from=` list. With `--tailscale`, an address that is not on the tailnet is refused:
  a name `tailscale ip` does not know, or a literal IP outside `100.64.0.0/10`. Without the flag nothing
  is checked; a `*.ts.net` name works like any host. `worker doctor` re-runs `tailscale ip` and reports a
  changed address. No Tailscale API key and no policy edits.
- Why `tailscale ip` rather than `tailscale status --json`: the CLI docs mark the JSON format "subject
  to change", while `tailscale ip <hostname>` is a documented one-address-per-line answer [CLI].
- Storing the IP keeps a DNS failure or a spoofed name from moving the connection off the tailnet;
  the pinned host key backs this up.

### 2.4 Surviving an SSH drop: Option B

| | A: detached keeper per lane | B: OMP ends, resume from session file |
| --- | --- | --- |
| Memory | One resident process per remote lane for its whole life, plus replay buffers | Nothing beyond `serve` while connected |
| Complexity | Detach, pid files, orphan cleanup when a lane closes while the hub is away, sequence numbers, replay, buffer overflow that falls back to the transcript anyway, keepers from an older worker binary still running after an update | Reuses `recover_one` (lease wait, admission, `--resume`, identity check), written and tested for decision 9 |
| In-doubt deliveries | Two mechanisms (sequence acks, transcript); a dead keeper still needs the second | One: the transcript search of section 9.3, unchanged; a write in flight at the drop is found or stays in doubt |
| While the hub is unreachable | The lane keeps running but every shell call is refused, because the bridge's veto fails closed after 2 s; dialogs and reports wait | Nothing runs and nothing burns tokens |
| User experience | A short blip is invisible | A drop longer than ~45 s cuts the running turn; the lane resumes on reconnect with one note |

**Choose B.** A pays for a process per lane and a second recovery mechanism, and still cannot work
safely without the hub. B is the same rule for local and remote lanes: after a hub restart or a drop,
every lane comes back from its session file. SSH keepalives and the 45 s heartbeat mean a gap shorter
than that does not drop at all. Over a tailnet, a roaming laptop keeps its tailnet IP, so its connections
should survive a network change inside that window [inf.].

On the worker, a closed stream (EOF, write error or 45 s silence) makes `serve` close every OMP child by
the HOST-20 rule (end input, 15 s, TERM, 5 s, KILL) and exit. A new `serve` for the same hub first takes
`serve.lock`; if an older one still holds it (half-open TCP), it sends that process TERM and waits up to
30 s. On the hub, the reattach waits for `omp session lease` on the worker to show the lease free, then
resumes. A resume that binds another session stays held for inspection, as now.

### 2.5 Bridge path and extension file

**Multiplex over the worker-mode stream.** `serve` creates `worker-<hub>.sock` in the per-user runtime
directory (same 0700 and peer-credential rules as `hub.sock`) and passes it to OMP as `CATALY_SOCKET`.
Each accepted connection becomes a channel to the hub. The hub treats it as a bridge connection *from
worker W*: only `bridge.*` methods; `bridge.hello` must carry a launch token or reconnect key issued
for a lane on W; anything else gets role `none`.

Rejected: `-R` Unix-socket forwarding. (1) `restrict` disables port forwarding; whether that covers Unix
sockets is not documented (the man page names TCP), and lifting `restrict` reopens TCP forwarding.
(2) The forwarded connection reaches `hub.sock` from the hub's own `ssh` process, so the
peer-credential check would give owner authority to any process on the worker. (3) After a drop, the
stale remote socket blocks a new forward unless the owner sets `StreamLocalBindUnlink yes` in the
worker's sshd, whose default is `no` [sshd_config(5)]. (4) It needs one forward per lane.

**Extension file: shipped by the worker's own `cataly`.** Installing Cataly on the worker already links
OMP's global extension folder to its `cataly.ts` (11.2); `worker init` checks the link and makes it for
npm and Bun installs, which otherwise make it at hub start. Rejected: a copy pushed by the hub. It would
add a write path into OMP's folder, fight the worker's own install link, and load in the owner's own OMP
sessions on that machine. Skew is caught by the bridge protocol number (2.8), which today is 1 and rarely
changes, because tools come from the hub.

### 2.6 Placement and repositories

- `lane.open` gains `worker` (a configured name). The hub refuses: an unknown worker; one that is not
  connected or cannot connect within 10 s ("offline since …"); a full one (`lanes` reached); an
  incompatible one; a tern lane.
- **Lead rule**, in its guidance: set `worker` only when the owner named that machine or a platform only
  it offers ("리눅스에서 테스트해" while the hub is a Mac), or the repository's origin is listed in that
  worker's `repos`. Never for load or speed alone. Two candidates: ask. `lane.workers` lists name, OS and
  arch, state, lanes in use, and repos.
- **Repository.** The lane's `cwd` is a hub checkout. The hub reads its `origin` URL and normalises it
  (`github.com/owner/name`). It resolves the commit: `worktree.base` if given, else `HEAD`. The worker
  keeps one clone per origin under `<root>/repos/`, fetches, checks that the commit exists there, and
  adds a worktree at `<root>/lanes/<key>` on the lane's branch. A commit that is not on the origin is
  refused ("push it first, or open the lane here"). `repos` is placement evidence and a doctor check
  list, not an allow-list: the worker fetches with its own Git credentials.
- **Not synced:** uncommitted edits and untracked files of the hub checkout. The open reply carries a
  warning when the hub checkout is dirty, and the thread shows the commit used.
- A remote lane counts toward `[lanes] max` (the lead's span) and the worker's `lanes`, not toward
  `[sessions] cap` (this machine's processes). The config key is `lanes`, not `slots`, because Slot
  already means a named heavy-command right.

### 2.7 Done checks

`command` runs through `exec` in the lane's worktree with today's bounds. `file` reads through `read`,
relative to the worktree, and the hash or JSON field is checked on the hub. `pr_merged`, `issue_closed`,
`web` and `pr_policy` stay on the hub with its `gh` login. The claim card names where each check ran. A
worker can lie about its own `command` and `file` results (2.9); merges still need the owner's
authorisation and `pr_merged` is read on the hub.

### 2.8 Version skew and updates

- **Compatible** = same worker protocol major in `hello` *and* the worker's bridge protocol (reported in
  `hello`, checked again in `bridge.hello`) equal to the hub's (today 1). Product versions may differ.
  Fields are only added within a major; unknown fields are ignored.
- Incompatible: no new lane is placed, a lost lane is not resumed, the worker shows
  "incompatible: update cataly on linux-box" in the panel and `worker list`, and the lead is told once.
  Nothing runs half-compatible.
- `cataly update` updates the hub only (decision 12). After the restart each worker's `hello` is checked
  again, and incompatible ones are listed in the update's output. Each worker updates itself with its
  own `cataly update`; its running `serve` keeps the old binary until the next connection, because the
  forced command uses the `current` link. `cataly worker update <name>` (phase 4) runs that `cataly
  update` over `exec`, only when the worker has no live lane. The worker's own trusted binary checks the
  signature, as in 11.4.

### 2.9 Failure and security

| Case | Behaviour |
| --- | --- |
| Worker disappears mid-lane | 45 s heartbeat → its lanes are `lost` ("linux-box unreachable"); deliveries in flight follow 9.3; one urgent `worker.offline` event; reconnect with backoff 1 s doubling to 60 s while any lane is lost on it |
| Worker reboots | Same as a drop. The OMP children are gone; worktrees and session files are on its disk; after sshd is up the hub reconnects and resumes |
| Hub restarts | Its `ssh` children end, so `serve` closes the OMP children; hub start recovers remote lanes like local ones |
| Host key changed | `ssh` refuses. The worker becomes `refused: host key changed`, lanes stay lost, the owner is told once. Fix: `worker init` again on the worker, `worker add … --host-key SHA256:…` on the hub. Never auto-accepted |
| Forced command missing | Marker command runs → `refused: no forced command` |
| Worker incompatible | 2.8 |

**Who can run what:**
- Whoever holds a hub key can run any program as the worker user. The forced command narrows the
  interface (no shell, PTY, forwarding, agent or rc), not the power: OMP's tools and `command` checks
  run arbitrary code by design.
- **OMP's tools on a worker run as the worker user, not in a sandbox.** A lane can read and change
  everything that user can: `~/.ssh`, browser profiles, other checkouts, that user's OMP logins. On a
  machine someone uses daily, enrol a dedicated OS user with its own OMP login. A second OS user on the
  hub machine is a valid worker and gives the same separation.
- A worker can send bridge traffic only for its own lanes, with lane authority (cannot open, close,
  merge, post to the owner or grant itself exceptions, 9.1). It never gets owner or CLI authority, never
  sees a relay, and can make its own `command` and `file` checks lie.
- Other users on the worker: the 0700 socket directory and peer-credential check, as on the hub. Other
  hosts: nothing Cataly listens on; sshd is the only entry, optionally bound to the tailnet address,
  with `from=` limiting the hub key to the hub's tailnet IPs.
- A lane's `cataly` CLI on a worker finds no hub; lanes use their `cataly_lane` tool. `cataly slot` on
  a worker comes in phase 4.

### 2.10 Rejected options

- Second hub per machine: two bots or a hub-to-hub protocol; the owner asked for one.
- SSH library in the hub: a crypto and SSH stack to own instead of the system's OpenSSH.
- Tailscale SSH as the transport: it ignores `authorized_keys`, so no forced command (2.3).
- Option A (detached keeper per lane): 2.4.
- `-R` socket forwarding for the bridge: 2.5.
- Hub-pushed extension copy: 2.5.
- Syncing uncommitted edits (rsync or patches): a second copy of the owner's tree with no clear owner.
- `ControlMaster` sharing: the worker stream already multiplexes; control sockets add files and states.
- Owner's `~/.ssh/config`: its forwardings or proxies could weaken the boundary (open question 5).

## 3. 사용자 흐름

**등록 (두 단계)**

1. 허브에서 `cataly worker add dev@linux-box --tailscale` 를 실행합니다.
   `tailscale ip`로 `linux-box`를 100.101.102.103 으로 찾고, 이 워커 전용 키를 만든 뒤 한 줄을 보여 줍니다:
   `워커에서 실행하세요: cataly worker init --hub-key 'ssh-ed25519 AAAA… cataly-hub-3f2a9c' --from 100.64.1.2`
2. 워커(cataly 설치, OMP 로그인 완료된 계정)에서 그 명령을 실행합니다. `authorized_keys`에 강제 명령 줄을
   넣고, `omp`·`git` 경로와 브리지 링크를 확인한 뒤 이렇게 안내합니다:
   `허브에서 실행하세요: cataly worker add dev@linux-box --host-key SHA256:Qm3…`
3. 허브에서 다시 `worker add`를 실행하면 호스트 키 지문을 비교해 고정하고, 접속해서 버전·OMP·브리지를 확인한 뒤
   `[workers.linux-box]`를 씁니다: `linux-box 등록 완료 · Linux x64 · cataly 0.3.0 (호환) · omp 19.2.0 · 레인 최대 1개`.
   저장소는 `--repo github.com/vntrevx/cataly-core`로 붙입니다.
   지문이 다르거나 강제 명령이 적용되지 않으면 등록하지 않습니다: "tailnet의 22번 포트를 Tailscale SSH가 받고 있으면
   이렇게 됩니다. OpenSSH를 tailnet 주소의 다른 포트(예: 2222)로 열고 `dev@linux-box:2222`로 다시 등록하세요."

**디스코드에서 원격 레인 요청**

- 주인: "cataly-core 리눅스에서 테스트 돌려줘"
- 리드: 허브가 macOS이고 Linux 워커는 linux-box 하나뿐 → `worker: linux-box`로 레인을 엽니다.
  "linux-box(리눅스)에서 `linux-test` 레인을 열었어요. origin의 `a1b2c3d` 기준입니다. 허브 쪽의 커밋 안 된
  수정 2개는 워커에 없어요."
- 기계 이름도 플랫폼도 없고 저장소도 어느 워커에 등록돼 있지 않으면 리드는 이 컴퓨터에서 엽니다. 후보가 둘이면 묻습니다.

**스레드에 보이는 것**

- 첫 글: `🖥️ linux-box (Linux x64) · a1b2c3d · 작업 폴더와 세션 파일은 워커에 있음`
- 진행·보고·질문 카드는 로컬 레인과 같습니다. 완료 체크는 실행 위치를 붙입니다:
  `✅ command cargo test (linux-box) · ⏳ pr_merged (허브)`.
- 운영 패널: `🛰️ 워커: linux-box 🟢 레인 1/1 · mac-mini ⚪ 쉬는 중 · old-box ⛔ 호스트 키 변경`.

**실패할 때**

- 연결 끊김: `⚠️ linux-box 연결이 끊겼어요 (14:02). 하던 턴은 중단됐고, 다시 연결되면 세션 파일에서 이어 갑니다.`
- 다시 연결: `🔄 linux-box 재연결 · linux-test를 세션 파일에서 재개했어요.` 끊기기 직전 보낸 메시지가 세션 파일에
  없고 다시 보내도 된다는 증거가 없으면, 리드가 풀 때까지 그 레인 입력을 막아 둡니다 (중복 전송 없음).
- 워커 재부팅: 위와 같습니다. 워커에 Cataly 서비스가 따로 필요 없습니다. sshd가 뜨면 이어집니다.
- 꺼진 워커에 새 레인: 리드가 "linux-box가 3시간째 연결되지 않아요. 여기서 열까요?"라고 묻습니다.
- 호스트 키 변경: `⛔ linux-box의 호스트 키가 바뀌어 연결을 거부했어요. 워커에서 cataly worker init을 다시 실행해
  지문을 확인한 뒤 허브에서 cataly worker add … --host-key …로 다시 고정하세요.` 자동으로 받아들이지 않습니다.
- 버전 비호환: `linux-box의 cataly가 워커 프로토콜 1을 말하지 않아요. 워커에서 cataly update를 실행하세요.`
- `--tailscale`인데 tailnet 주소가 아님: `linux-box.example.com은 tailnet에 없어요. 등록하지 않았어요.`

## 4. Changes

### 4.1 Modules

| Module | Change |
| --- | --- |
| `worker` (new, about four files) | `mux` (frames, channels, credit, heartbeat), `link` (hub: `ssh` child, connect, backoff, channel-0 client, `Place::Worker`), `serve` (worker: forced-command entry, `serve.lock` takeover, the nine methods, bridge socket), `enrol` (keys, `authorized_keys` line, pins, `tailscale ip`) |
| `sys::command` | Unchanged; `worker::Place` calls it locally or forwards the same `Spec` over `exec` |
| `omp::transcript` | Reads through a byte source (local file or `read`); every parser unchanged |
| `omp::launch` | Remote session env: no hub credentials, `CATALY_SOCKET` = worker socket; overlay and instructions written to the worker root |
| `host::headless` | `start` takes a `Place`; no PID adoption for remote children; stderr log from the channel, 1 MiB cap |
| `hub::bridge`, `hub::socket` | Bridge connections from worker channels: `bridge.*` only, tokens scoped to the worker |
| `lanes` | `OpenInput.worker`, `Record.worker`; validation (2.6); worktree prepare, unsent check, fingerprint and removal through `Place`; check routing; drop → `lost`; reattach through `recover_one` |
| `hub::tool` | `lane.open` `worker?`; `lane.workers`; placement rule in the lead's guidance |
| `config` | `[workers.<name>]` parse, validate, reload; written with `cli::setup::set_key` |
| `jobs::ops` | Panel line per worker; events `worker.offline` (urgent), `worker.online` (quiet), `worker.refused` (urgent) |
| `cli` | `worker` verbs (4.2); `worker serve` refuses to run on a terminal |
| `extension/cataly.ts` | No change |
| Docs | `DESIGN.md` 2.1, 4, 8 and 11.2; `docs/config.md` `[workers]`; no new dependency (OpenSSH, like `git` and `gh`, is a system tool) |

### 4.2 CLI verbs

| Verb | Where | Does |
| --- | --- | --- |
| `cataly worker add user@host[:port] [--name N] [--tailscale] [--host-key SHA256:…] [--repo ORIGIN]… [--lanes N]` | Hub | First run: key and `init` line. With `--host-key`: pin, connect, verify, write config, reload the hub |
| `cataly worker init --hub-key '…' [--from IPs] [--host-key-file F] [--remove]` | Worker | Write or remove the forced-command line; check `omp`, `git`, bridge link; print the fingerprint and the hub command |
| `cataly worker list [--json]` | Hub | Name, destination, state, version, compatibility, lanes in use/max, last seen |
| `cataly worker doctor [NAME]` | Hub | Connect; check pin, marker, protocols, OMP probe, provider login, `git ls-remote` per repo, disk; `tailscale ip` when set |
| `cataly worker remove NAME` | Hub | Refused while lanes live; `forget` on the worker if reachable, else print the line to delete; drop key, pin, config |
| `cataly worker update NAME` (phase 4) | Hub | The worker's own `cataly update`, idle workers only |
| `cataly worker serve --hub ID` | Worker, sshd only | Worker mode |

### 4.3 Config, files, store, protocols

```toml
[workers.linux-box]
destination = "dev@100.101.102.103"   # user@host, written by `worker add`
port = 2222                           # default 22
name = "linux-box"                    # display; the tailnet machine name with --tailscale
tailscale = true                      # doctor re-checks the address with `tailscale ip`
repos = ["github.com/vntrevx/cataly-core"]   # placement evidence and doctor checks
lanes = 1                             # remote lanes at once, 1-8
```

- Hub files: `<config>/secrets/worker-<name>` (0600, out of backups like every secret),
  `<config>/known_hosts` (0600). Worker files: `<config>/worker.toml` (`omp` path, `PATH` captured by
  `init`), root `<state>/worker/<hub id>/` with `repos/`, `lanes/`, `sessions/`, `serve.lock`.
- Store, migration `0010_workers.sql` (additive): `workers (name PK, state, reason, version, protocol,
  bridge, os, arch, omp, seen_ms, changed_ms)` and `sessions.worker TEXT NULL`. The lane record is a
  JSON column, so `worker` is a new field with a default and needs no SQL.
- Protocols: new worker protocol v1 (2.2) in `proto`. Bridge protocol unchanged (1); its hello gains
  nothing. Control protocol gains `worker.list`, `worker.doctor` and `worker.remove` (owner authority);
  `worker add` applies its config change with the existing `config reload`.

## 5. Tests

| Level | What | Linux | macOS |
| --- | --- | --- | --- |
| Unit | Frame codec (property: round trip, oversize refused), credit accounting, `authorized_keys` render and replace (applying twice changes nothing), `[host]:port` pin, origin normalisation (property), placement validation, check routing, compatibility table, `tailscale ip` parsing | yes | yes |
| Loopback sshd | Each test starts an unprivileged `sshd -D -f <cfg> -h <key> -p <free port>` as the runner user; sshd(8) requires `-h` when not run as root, and that such an sshd accepts only its own user is checked first in phase 1 [inf.]. Separate `HOME`/`XDG_*` make hub and worker two Cataly homes. Stock OMP with the loopback fake model and a `file://` bare repo as origin | CI | CI |
| Scenarios on it | Enrolment (both steps); marker refused when `command=` is absent; changed host key refused (new key, restart sshd); remote lane open → `command` check → claim → close; kill the `ssh` child mid-turn → lost → reattach → resumed on the same session; delivery in flight at the drop → found or fenced, never resent; `serve` takeover; SIGKILL of `serve` and its OMP ("reboot"); fake worker on protocol 2 → incompatible | CI | CI |
| Container | `ubuntu-latest` only: a container with sshd on 22 and a separate `worker` user and filesystem; restart it for a real reboot; regenerate its host keys | CI | no (no Docker on macOS runners) |
| Two OS users | `scripts/worker-two-users.sh` for a developer machine (sudo: create a user, enable sshd) | manual | manual |
| Contract | The contract suite's session cases once more through `Place::Worker` over loopback sshd | CI | CI |
| Benchmarks | Hub RSS with one worker and two remote lanes vs none; `serve` RSS; lane delivery latency over loopback SSH | CI | CI |

**Tailscale:** CI cannot join a tailnet. Tested: `--tailscale` resolution and refusal with a fake
`tailscale` on `PATH`; the `from=` line; the marker refusal, which is what a server ignoring
`authorized_keys` (Tailscale SSH) looks like; pins on a non-22 port. Not tested in CI: real MagicDNS,
Tailscale SSH taking port 22, sshd bound to a tailnet IP at boot, keeping the connection while the
network changes. Before the release that ships workers, a manual checklist runs on the owner's tailnet
with a Linux worker that has Tailscale SSH on.

## 6. Phases

Sizes are product Rust lines plus test lines **[est.]**.

1. **Enrol and connect (smallest useful slice), ~900 + 500.** `worker` module (`mux`, `link`, `serve`
   with `hello`/`status`/`exec`/`read`/`stat`/`write`/`forget`), `worker add/init/list/doctor/remove`,
   `--tailscale`, config, migration, loopback-sshd harness on both OSes. Useful alone: `worker doctor`
   proves a machine is ready (pin, boundary, OMP login, Git access) before any lane uses it.
2. **Remote lanes, ~1,300 + 700.** `Place` seam (headless start, transcript source, git, checks),
   `spawn`/`signal`, bridge channels, `lane.open worker`, repository prepare, thread, board and panel
   lines, `lane.workers` and the lead rule. A drop marks lanes lost; the lead reopens by hand. Contract
   cases over `Place::Worker`.
3. **Survive drops, ~400 + 400.** Automatic reattach through `recover_one`, `serve.lock` takeover,
   host-key and incompatibility states, events, Linux container job, benchmarks recorded in
   `docs/measurements.md`.
4. **Worker capacity and accounts, ~500 + 300.** Per-worker quota (`omp usage --json` over `exec`, the
   gate held per worker), admission from worker memory and disk, `cataly slot` on workers, `cataly
   worker update`.

## 7. Open questions for the owner

1. Repositories the hub has no checkout of: may the lead name `repo` and a remote ref directly (proposed:
   not in phase 2)?
2. A lane asked for on an offline worker: refuse and ask (proposed), or queue it until the worker connects?
3. Reattach after a drop: always automatic like decision 9 (proposed), or only within a window (say 24 h)
   with the lead deciding after that?
4. Should `cataly worker update` exist at all (phase 4), or does each worker stay updated by hand?
5. `-F none` rules out `ProxyJump` and bastions. Add a pinned `jump` key later, or keep workers
   reachable directly (LAN or tailnet) only?
6. Dedicated OS user on a worker: recommend (proposed) or require for machines a person uses daily?
7. API keys that live only in a worker's environment: carry a list in `worker.toml`, or rely on OMP's
   stored logins only (proposed)?
8. Owner-chosen checkout on the worker instead of Cataly's own clone under its root (proposed: own clone only)?

## Sources

- sshd(8), AUTHORIZED_KEYS and SSH_KNOWN_HOSTS formats, `-h`: https://man.openbsd.org/sshd.8
- sshd_config(5), `AllowStreamLocalForwarding`, `StreamLocalBindUnlink`: https://man.openbsd.org/sshd_config.5
- ssh(1), `-F none`: https://man.openbsd.org/ssh.1 · ssh_config(5): https://man.openbsd.org/ssh_config.5
- Tailscale SSH: https://tailscale.com/kb/1193/tailscale-ssh
- MagicDNS: https://tailscale.com/kb/1081/magicdns
- 100.x addresses: https://tailscale.com/kb/1015/100.x-addresses
- Tailscale CLI (`tailscale ip`, `status --json` warning): https://tailscale.com/kb/1080/cli
