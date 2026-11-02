# omp-dori 설계

작성일: 2026-11-02
원본: `ref/dori-superagent/skills/dori-mode/` (Dori 모드), `ref/omp-ulw/` (omp용 omo 프롬프트 확장)

## 1. 의도

사용자는 Dori 모드를 매일 쓰고 싶다. Dori 모드는 메신저(Telegram, Discord, Slack)로 일을 받아 herdr 탭마다 코딩 에이전트 세션(레인)을 띄우고, 증거로 완료를 확인할 때까지 따라가는 상시 비서다. 원본은 omo(senpi) 위에서 돈다.

omp-dori는 Dori 모드를 **원본과 최대한 똑같이** 동작시키되, 다음 한 가지만 바꾼다.

> Dori 세션과 모든 레인이 omp에서 돌고, 각 세션의 모델 프롬프트는 omp-ulw가 omp용으로 옮긴 omo(Senpi) 프리셋을 쓴다.

판단 기준은 "원본 Dori 모드 사용자가 차이를 느끼지 않는가"이다. 원본에 있는 동작은 빼지 않고, 원본에 없는 동작은 더하지 않는다.

## 2. 목표 / 비목표 / 반목표

**목표**
- Dori 모드 스킬(`SKILL.md`, `references/`)과 `dori` CLI(`scripts/`)의 모든 명령이 omp 환경에서 원본과 같은 결과를 낸다. 대상 명령은 launch, adopt, sync, claim-done, object-done, close, pause, resume, watch, freshness, dead-panes, guard, heavy, can-launch, send, presence, transcribe, inbound다.
- Dori 세션과 레인 세션 모두 omp-ulw를 로드해 모델별 omo 프리셋으로 돈다.
- 레인은 원본처럼 goal을 걸고 끝날 때까지 계속 일한다. pause와 resume은 그 goal을 멈추고 다시 켠다.
- 원본 Dori 세션이 기대하는 "상시 모니터"(명령 출력 줄이 필터에 맞으면 세션을 깨움)를 omp에서 제공한다.
- GitHub 공개 저장소로 배포한다. 다른 사람이 자기 맥이나 Linux에서 원본처럼 한 줄 설치(`curl … | bash`)로 쓸 수 있어야 한다.
- Dori와 무관한 평소 omp 세션은 설치 전과 똑같이 돈다. 레인이 재시작되거나 재개되는 경로(herdr 서버 재시작 후 자동 복원, 닫힌 레인 다시 열기, 죽은 pane 다시 띄우기)에서도 Dori·레인 세션은 omo 프리셋과 omp-dori 기능을 유지한다.

**비목표**
- `dori-superagent` 루트 하네스(`src/`의 lead/lane/duty/merge 하네스)는 옮기지 않는다.
- omp-ulw 코드는 수정하지 않는다. 경로로 가리키는 외부 구성요소로만 쓴다.
- omo 호스트 지원을 유지하지 않는다. 설정으로 omo 명령을 넣을 수는 있지만 검증 대상이 아니다.
- 새 기능, 새 메신저, UI는 만들지 않는다.

**반목표** (기술적으로는 돌아가도 실패로 본다)
- 원본과 다르게 동작하는 것. 예: 레인이 goal 없이 돌아서 pause가 아무것도 멈추지 않는 경우, 완료 주장이 증거 확인 없이 닫히는 경우.
- 확인하지 않은 채 성공으로 보고하는 것. 원본은 PAUSE_UNVERIFIED, STARTUP_UNCONFIRMED처럼 실패를 정직하게 드러낸다. omp 이식 때문에 확인 경로가 깨지면 "UNVERIFIED"가 항상 나오거나, 더 나쁘게는 항상 성공으로 나와서는 안 된다.
- Dori와 무관한 omp 세션의 시스템 프롬프트, 모델 프리셋, 도구, 동작이 바뀌는 것.
- 설치한 사람의 omp 로그인 정보, 기존 세션 기록, 다른 확장(herdr 통합 포함)과 분리돼 따로 설정해야 하는 것.

## 3. 제약과 확인된 사실

| 항목 | 사실 | 근거 |
|---|---|---|
| omp 버전 | 설치본 18.12.2. omp-ulw의 기준은 18.14.2(`host.ts`·`payload.ts` 치환 문구가 이 릴리스와 일치) | `omp --version`, `ref/omp-ulw/README.md:88-93` |
| omp CLI | `--model`, `-e/--extension`(여러 번 가능), `--append-system-prompt`, `-p`, `--no-session`, `--resume`, `--session-dir`, `--goal=<objective>`가 있다. `--goal`은 새 세션을 goal 모드로 시작한다. 대화형 전용이고 위치 인자 프롬프트와 같이 쓸 수 없다. `--session` 플래그는 **없다** | `omp --help`, omp `src/cli/args.ts:125-182` |
| 첫 지시와 goal | 위치 인자로 넘긴 첫 지시는 `input` 이벤트를 거치지 않는다. 그래서 omp-ulw의 `ulw`→`/goal` 변환이 일어나지 않고, 그것만으로는 goal이 생기지 않는다. 확장 API로 goal을 직접 만들 수는 없다(`goal_updated` 수신만 가능). 다만 ULW 작동은 첫 턴에서도 일어난다(확장에서 온 입력이 아니므로) | omp `src/main.ts:797-804`, `ref/omp-ulw/ulw.ts:115,131-134,155-163`, omp `src/extensibility/extensions/types.ts:1682` |
| omp `goal` 도구 | 에이전트가 부르는 `goal` 도구가 있고 `op: create/get/complete/resume/drop`, `objective`를 받는다. 기본 도구 목록에는 없다. `goal.enabled`(기본 true)이고 goal 기록이 아직 없는 세션에서는 확장이 `setActiveTools`로 켤 수 있다 | omp `src/goals/tools/goal-tool.ts:22-23,91-93`, `src/tools/index.ts:733-742`, `src/goals/settings.ts:3-6` |
| 원본의 goal 생성 방식 | 원본 레인은 시작 문구(`ulw set goal and work. …`)를 일반 첫 메시지로 받는다(`launch.ts:58-60`). omo의 goal 확장(pi-goal)에는 에이전트용 `create_goal({ objective })` 도구가 있다. 그래서 레인 에이전트가 브리프를 읽고 objective를 직접 써서 goal을 만든다. [추정: senpi 실행으로 확인하지 않음. pi-goal README에 "create_goal may store a model-inferred objective"라고 적혀 있다] | https://github.com/code-yeongyu/pi-goal , `ref/dori-superagent/skills/dori-mode/scripts/src/launch.ts:58-60` |
| omp 스킬 경로 | `~/.agents/skills`를 읽는다(원본 install.sh 기본값과 같음) | omp `src/discovery/agents.ts:30,177` |
| omp 세션 저장 위치 | `~/.omp/agent/sessions/<인코딩된 cwd>/<ISO>_<uuid>.jsonl`. 홈 아래 cwd는 `-dev-x`처럼 홈 기준 상대경로로, 임시 폴더는 `-tmp...`로 인코딩한다. 원본이 가정한 `--Users-...--` 규칙과 **다르다**. 파일명 형식은 원본 정규식과 맞는다 | omp `src/session/session-paths.ts:78-110`, 실제 디렉터리 |
| omp 프로세스 | `bun /…/.bun/bin/omp …`로 보인다. 실체는 `@oh-my-pi/pi-coding-agent/dist/cli.js` | `ps`, `readlink` |
| `PI_SESSION_ID` | omp 소스에 없다. 원본의 child-env 경로는 omp에서 항상 실패한다 | omp `src/` 검색 |
| omp `/goal` | `set/show/pause/resume/drop/budget` 하위 명령이 있다. TUI 전용이다. 출력 문구는 `Goal mode paused.`, `Goal mode resumed.`, `No active goal to pause.`, `No paused goal to resume.`이다. senpi의 `Goal paused`/`Goal active`와 다르다. `No active goal to pause.`는 goal이 없을 때와 이미 멈춰 있을 때 둘 다 나온다 | omp `src/modes/interactive-mode.ts:565-567,5339,6492,6501,6505` |
| omp 확장 API | `registerCommand`, `registerTool`, `registerFlag`, `on(event)`, `sendMessage(msg, {triggerTurn, deliverAs})`, `sendUserMessage`, `appendEntry`가 있다. 외부 이벤트로 세션을 깨우는 예제가 `examples/hooks/file-trigger.ts`에 있다 | omp `src/extensibility/extensions/types.ts:1764-1844` |
| herdr | 공식 지원 에이전트에 OMP가 있다. 상태(idle/working/blocked)는 `herdr integration install omp`가 설치하는 전역 omp 확장(`~/.omp/agent/extensions/herdr-omp-agent-state.ts`)이 보고한다. 이 보고가 있으면 herdr는 화면 감지 규칙을 쓰지 않는다. 이 통합은 omp의 세션 ID도 herdr에 보고한다. 이 맥에는 herdr가 아직 없다 | https://herdr.dev/docs/agents/ , https://herdr.dev/docs/integrations/ , `command -v herdr` |
| omp-ulw ULW 끄기 | `--omo-ultrawork-disabled`, `--omo-ulw-workflows-disabled`, `--omo-ulw-continuation-disabled` 플래그가 있다 | `ref/omp-ulw/ulw.ts:83-85` |
| omp-ulw 로딩 | 전역 확장 디렉터리에 두면 모든 새 omp 세션이 자동으로 로드한다. 기본 export는 `ompUlw(pi)` 함수 하나다 | `ref/omp-ulw/index.ts:1-3,52` |
| omp-ulw 세션 상태 복원 | ULW 작동 여부를 `pi.appendEntry`로 세션 파일에 기록하고, `session_start`/`session_switch` 때 세션 브랜치에서 다시 읽는다. 그래서 `--resume`으로 다시 떠도 유지된다 | `ref/omp-ulw/ulw.ts:100-108` |
| omp 프로필 | `--profile`/`OMP_PROFILE`은 "isolated agent state"다. [추정: 별도 agent 디렉터리를 쓰므로 로그인 정보, 세션 기록, 전역 확장(herdr 통합 포함)이 분리된다. 그래서 4.2에서 이 방식을 쓰지 않는다] | omp `src/cli.ts:446-459`, `src/cli/help-extra.ts:66` |
| 라이선스 | 원본 dori-superagent는 MIT(Copyright 2026 YeonGyu Kim). omp-ulw 저장소(`github.com/dbc-hbin/omp-ulw`)에는 최상위 LICENSE 파일이 없다. 프리셋 파일은 MIT(pi-mono, senpi) 고지를 담고 있다 | `ref/dori-superagent/LICENSE`, `ref/omp-ulw/` 목록, `ref/omp-ulw/upstream/presets.js` 머리말, `ref/omp-ulw/README.md:10` |

### 원본에서 omo에 묶인 곳 (전수)

| 위치 | 내용 | omp에서 필요한 것 |
|---|---|---|
| `scripts/src/config.ts:48` | `agentCommand: ["omo","--model","{model}","{prompt}"]` | 기본값을 omp + omp-ulw로 바꾼다(4.2) |
| `scripts/src/config.ts:51` | `sessionsDir: ~/.omo/agent/sessions` | `~/.omp/agent/sessions` |
| `scripts/src/session-id.ts:9-11,33` | 프로세스 판별 정규식(omo/senpi), `--session` 인자, `PI_SESSION_ID`, 세션 폴더 인코딩 | omp용으로 다시 만든다(4.4) |
| `scripts/src/pause.ts:13-17` | senpi `/goal` 출력 문구로 pause/resume 확인 | omp용 확인 경로(4.5) |
| `scripts/src/launch.ts:21` | 시작 실패 패턴(`STARTUP_FAILURE`) | omp 실패 문구가 다르면 추가한다(4.3) |
| `references/config.example.json`, `references/scripts.md:28-29`, `references/sessions.md:10`, `references/setup.md` 1·4·5절 | omo 명령, `herdr integration install pi`, "your monitor tool" | 문서를 omp 기준으로 고친다(4.6) |

## 4. 결정한 설계

### 4.1 저장소 구성

omp-dori는 원본 `skills/dori-mode/`를 가져와 고쳐 쓰는 fork다. 원본의 git 히스토리를 upstream으로 연결해 변경을 골라 받아올 수 있게 둔다. omp-ulw는 fork하지 않는다.

```
omp-dori/
  skills/dori-mode/        # 원본 SKILL.md, references/, assets/, scripts/를 omp용으로 고친 것
  extension/               # 새로 만드는 omp 확장: omp-dori (4.2 ~ 4.6)
  install.sh               # omp용 설치 스크립트 (4.7)
  README.md, LICENSE, docs/
  ref/                     # 개발 중 참조용 원본. 배포 저장소에는 넣지 않는다
```

원본 Dori 모드의 동작 규칙(SKILL.md 본문, done 프로토콜, 레지스트리 상태 전이, 신호 문법, 메신저 코드)은 바꾸지 않는다. 호스트에 묶인 문장만 고친다.

### 4.2 세션 역할과 확장 로딩

요구: 평소 omp 세션은 그대로 두고, Dori·레인 세션은 어떤 경로로 다시 떠도 omo 프리셋과 omp-dori 기능을 유지한다.

검토한 방법:
- `-e`로만 로드: herdr 자동 복원(`omp --resume=…`) 등 `-e` 없이 다시 뜨는 경로에서 기능이 빠진다. 탈락.
- omp-ulw 전역 설치: 평소 세션이 바뀐다. 탈락.
- omp 전용 프로필(`OMP_PROFILE`): 로그인 정보와 herdr 통합 확장이 분리될 것으로 보인다(3절). 탈락.

결정: **omp-dori 확장 하나만 전역 확장 디렉터리(`~/.omp/agent/extensions/`)에 설치한다.** herdr 통합과 같은 방식이다. omp-dori는 세션마다 "역할"을 판단하고, 역할이 없는 세션에서는 아무것도 하지 않는다.

- **역할 표시**: omp-dori가 CLI 플래그 `--dori-role <dori|lane>`을 등록한다. 이 플래그로 시작한 세션은 `session_start`에서 `pi.appendEntry("omp-dori:role", …)`로 역할을 세션 파일에 기록한다. 이후 `session_start`/`session_switch`가 일어나면 세션 브랜치에서 역할을 다시 읽는다. 그래서 `--resume`이나 herdr 자동 복원처럼 플래그 없이 다시 떠도 역할이 유지된다. omp-ulw가 ULW 상태를 복원하는 방식(`ulw.ts:100-108`)과 같다.
- **omp-ulw 적용**: omp-dori가 omp-ulw의 `index.ts`를 import해서 기본 export `ompUlw(pi)`를 호출한다. 이때 진짜 `pi`가 아니라 감싼 `pi`를 넘긴다. 감싼 `pi`는 omp-ulw가 실행 중에 효과를 내는 모든 진입점을 역할이 있는 세션에서만 실행한다.
  - `on(event, handler)`: 역할이 없으면 handler를 실행하지 않는다.
  - `registerCommand(name, { handler })`: handler를 감싼다. omp-ulw의 명령 handler는 `pi.sendUserMessage`를 직접 호출한다(`ulw.ts:124-135`). 그래서 이벤트만 막으면 평소 세션에서 `/ulw`를 입력했을 때 프롬프트가 들어간다. 역할이 없으면 handler는 프롬프트를 보내지 않고, "Dori 세션에서만 쓸 수 있다"는 알림만 띄운다.
  - `registerTool`을 쓰는 경우 같은 방식으로 실행부를 감싼다.
  - 로드 시점 등록(`registerFlag`, 명령·도구 등록 자체)은 그대로 통과시킨다.
  - 역할 판단은 세션 전환 이벤트(`session_start`, `session_switch`, `session_branch`, `session_tree`)마다 다시 한다. omp-dori의 역할 갱신 handler를 omp-ulw보다 먼저 등록해서, 같은 이벤트 안에서 omp-ulw의 복원 handler가 갱신된 역할을 보게 한다. 한 omp 프로세스 안에서 Dori 세션과 평소 세션 사이를 `/resume` 등으로 오가도 각 세션이 자기 역할대로 동작해야 한다.
  그래서 omp-ulw의 프리셋 주입, provider 요청 보정, `ulw` 입력 변환, 이어가기, 명령은 Dori·레인 세션에서만 작동한다. omp-ulw 코드는 고치지 않는다.
- **서브에이전트**: Dori·레인 세션이 띄운 서브에이전트는 omp-ulw가 원래 하던 대로 자식용 프리셋을 받아야 한다. 평소 세션의 서브에이전트는 받지 않는다. 자식 세션에서 부모의 역할을 알아내는 방법은 구현자가 정한다.
- **남는 차이**: omp-ulw의 명령(`/ulw`, `/review-work` 등)과 플래그는 세션 역할을 알기 전, 로드 시점에 등록된다. 그래서 평소 세션의 명령 목록과 `--help`에 보일 수 있다. 실행해도 위 감싸기 때문에 알림만 뜨고 아무 일도 일어나지 않는다. 구현자는 목록에서 감출 방법이 있으면 쓰고, 없으면 이 차이를 README에 적는다. [내 결정: 프롬프트와 동작이 바뀌지 않는 것이 핵심이다. 목록에 항목이 보이고 실행하면 안내가 뜨는 것까지는 받아들일 수 있는 잔여 차이로 본다.]
- **실행 명령**: 레인은 `config.agentCommand` 기본값 `["omp","--dori-role","lane","--model","{model}","{prompt}"]`로 띄운다. 원본처럼 시작 문구를 일반 첫 메시지로 넘긴다(4.3). `fill`과 `quoteForPane`의 인자 배열 처리는 원본 그대로 쓴다(`launch.ts:60`). Dori 세션은 사용자가 herdr pane에서 `omp --dori-role dori`로 띄운다. setup 문서와 install.sh가 이 명령을 안내한다.
- **omp-ulw의 ULW 플래그**: Dori·레인 세션에서 끄지 않는다. 원본 레인은 `launchKeywords: "ulw set goal and work"`로 omo의 ULW와 goal을 함께 켜고 돈다. 같은 동작을 재현하려면 ULW 작동이 필요하다. [내 결정: 사용자는 "기존 dori처럼, 다만 omp와 omo 프롬프트로"라고 했다. ULW를 켜 두는 것이 원본 omo 레인과 가장 가깝다고 판단했다.]

설정 키 추가: `ompUlwPath`(omp-ulw `index.ts` 절대경로). omp-dori 확장은 `~/.dori/config.json`(또는 `DORI_CONFIG`)에서 이 값을 읽는다. 값이 없거나 import에 실패하면 Dori·레인 세션에서 경고를 띄우고, omp-ulw 없이 나머지 omp-dori 기능만 켠다. 평소 세션은 어떤 경우에도 영향을 받지 않는다.

### 4.3 레인 goal 보장

원본의 핵심 동작은 "레인이 시작하면 goal이 걸리고 끝날 때까지 계속 돈다"는 것이다.

- 시작 문구는 원본 `launchKeywords` 그대로 쓴다(`ulw set goal and work. Read and execute the lane brief at …`).
- 원본과 같은 방식으로 goal을 만든다(사용자 결정). 시작 문구를 원본처럼 일반 첫 메시지(위치 인자)로 넘기고, 레인 에이전트가 브리프를 읽은 뒤 `goal` 도구의 `op: "create"`로 objective를 직접 써서 goal을 만든다. 원본의 `create_goal`과 같은 흐름이다(3절 "원본의 goal 생성 방식").
- omp의 `goal` 도구는 기본 도구 목록에 없다. 그래서 omp-dori가 역할이 `lane`이고 goal 기록이 아직 없는 세션에서 `setActiveTools`로 `goal` 도구를 켠다(3절 "omp `goal` 도구"). goal이 만들어진 뒤에는 omp가 도구 노출을 스스로 관리한다. Dori 세션과 평소 세션에서는 켜지 않는다. 원본 Dori 세션이 goal을 쓰는지는 확인되지 않았으므로 레인에만 적용한다.
- ULW: 시작 문구가 위치 인자로 들어가므로 첫 턴은 확장에서 온 입력이 아니다. 그래서 omp-ulw가 `ulw` 키워드로 ULW를 켠다(`ref/omp-ulw/ulw.ts:155-163`). 실제 실행으로 확인한다.
- **요구사항**: 레인 세션은 (1) 첫 턴에서 omp-ulw의 ULW 지침이 켜지고, (2) 에이전트가 직접 쓴 objective로 활성 omp native goal을 만들고, (3) goal이 끝날 때까지 omp goal 이어가기로 계속 일한다.
- omp-dori 확장은 레인 세션의 goal 상태(`goal_updated` 이벤트)를 `{stateDir}/sessions/<pane>.json`에 기록한다(4.4와 같은 파일).
- `STARTUP_OK` / `STARTUP_UNCONFIRMED` / `STARTUP_ERROR` 판정은 원본 그대로 둔다(herdr 상태와 화면의 실패 문구). 에이전트가 브리프를 읽고 goal을 만들기까지 20초 넘게 걸릴 수 있어서, 시작 확인 시점에 goal을 요구하지 않는다. [내 결정: 원본도 시작 확인에서 goal을 보지 않는다.]
- `STARTUP_FAILURE` 패턴에 omp의 시작 실패 문구(확장 로드 실패, 모델·인증 오류)를 추가한다. 기존 패턴은 남긴다.

### 4.4 레인 세션 ID 확인

원본 `resolveSession`은 pane → 셸 PID → 에이전트 프로세스 → 세션 ID 순으로 찾는다. 세션 ID를 얻는 방법은 `--session` 인자, `PI_SESSION_ID`, 시작 시각과 세션 파일 대조, 셋 중 하나다. omp에서는 앞의 둘이 없고, 셋째는 폴더 인코딩이 다르다.

결정:
1. **1순위**: omp-dori 확장이 레인 세션 시작 시(`session_start`) `{stateDir}/sessions/<pane>.json`에 `{ paneId, sessionId, sessionFile, cwd, startedAt }`을 쓴다. pane ID는 herdr가 넣어 주는 `HERDR_PANE_ID` 환경변수에서 읽는다(원본 루트 하네스도 이 변수를 쓴다: `ref/dori-superagent/src/cli.ts`의 `role` 명령). `resolveSession`은 이 파일을 먼저 읽고, 결과를 `via: "extension"`으로 표시한다.
2. **2순위(원본 방식 유지)**: 프로세스 판별 정규식에 omp를 추가한다(`(^|/)omp(\s|$)`, `pi-coding-agent/dist/cli.js`). 시작 시각 대조는 omp의 실제 폴더 인코딩 규칙(홈 기준 상대, 임시 폴더, 그 밖의 절대경로)을 따른다.
3. `--session` 인자와 `PI_SESSION_ID` 경로는 omp에서 의미가 없으므로 지운다.

`SessionHit.via` 타입에 `"extension"`을 추가한다. 레지스트리와 sync가 이 값을 그대로 기록한다.

herdr omp 통합도 세션 ID를 herdr에 보고한다. herdr CLI로 그 값을 읽을 수 있으면 구현자는 그것을 1순위로 써도 된다(`via: "herdr"`). 그래도 확장 상태 파일은 goal 상태와 승인 대기 상태(4.5, 4.8) 때문에 여전히 필요하다. [내 결정]

### 4.5 pause / resume 확인

원본은 `/goal pause`를 pane에 보내고, 화면에 `Goal paused` 줄이 하나 더 생겼는지로 확인한다. 확인하지 못하면 `PAUSE_UNVERIFIED`(exit 3)를 낸다.

결정: 보내는 명령은 원본과 같은 `/goal pause`, `/goal resume`이다(omp에 같은 하위 명령이 있다). 확인은 omp-dori 확장이 `{stateDir}/sessions/<pane>.json`에 기록한 goal 상태(4.3)를 읽어서 한다. 화면 문구는 보조로만 쓴다. omp의 `No active goal to pause.`는 goal이 없을 때와 이미 멈춰 있을 때를 구분하지 못하기 때문이다. [내 결정] 다음 조건을 지킨다.
- goal 상태가 paused로 바뀌었을 때만 `PAUSED`를 낸다. 확인하지 못하면 `PAUSE_UNVERIFIED`다.
- goal이 없는 레인은 원본처럼 확인된 것으로 본다. 원본에서 `cannot update goal: no goal exists`를 성공으로 치는 것과 같다.
- resume도 같은 방식이다. goal 상태가 active로 바뀌어야 `RESUMED`다.
- 실행 중인 턴은 원본처럼 Escape로 끊는다.

### 4.6 상시 모니터 (omp-dori 확장의 Dori 세션용 기능)

원본 setup 5절은 Dori 세션 안에 명령들을 "persistent monitor"로 걸어 두고, 출력 줄이 필터 정규식에 맞을 때마다 세션이 반응하길 기대한다. 대상은 메신저 수신, `dori watch`, `dori dead-panes --loop 3`, `dori guard --loop 1`, `dori freshness --loop 5`, `dori inbound slack --loop 1`, `dori presence ...`이다. omp에는 이 기능을 하는 기본 도구가 없다.

omp-dori 확장이 도구 하나를 등록한다. 이름은 `monitor`다.
- 동작: `start(name, argv, filter)`, `stop(name)`, `list()`. 명령은 인자 배열로 실행한다(셸 문자열 금지. 원본 Standing rules와 같다). stdout과 stderr를 줄 단위로 읽고, `filter`에 맞는 줄을 `pi.sendMessage({ customType: "dori-monitor", content: "[<name>] <line>", display: true }, { triggerTurn: true, deliverAs: "aside" })`로 세션에 넣는다. `aside`를 쓰는 이유는 진행 중인 도구 묶음을 끊지 않으면서, 세션이 쉬고 있으면 새 턴을 시작하기 때문이다.
- 모니터 프로세스가 죽으면 그 사실을 같은 방식으로 세션에 알린다. 다시 띄우는 것은 Dori의 판단에 맡긴다(원본에는 자동 재시작 규칙이 없다).
- 세션이 끝나면 띄운 모니터 프로세스를 모두 정리한다.
- 같은 이름으로 두 번 시작하지 못한다.
- 짧은 시간에 줄이 몰리면 한 메시지로 묶어 보낸다. 묶는 기준은 구현자가 정한다.
- 평소 세션의 도구 목록을 바꾸지 않도록 `defaultInactive: true`로 등록한다. 역할이 `dori`인 세션에서만 `setActiveTools`로 켠다(omp `types.ts:873-875,1859`). 레인 세션에서는 켜지 않는다. 원본에서 모니터는 Dori 세션만 쓴다.

SKILL.md와 `references/setup.md`의 "your monitor tool" 문장은 이 `monitor` 도구를 가리키도록 고친다.

### 4.7 설치

`install.sh`는 원본 흐름(`curl -fsSL …/install.sh | bash`, 다시 실행하면 갱신)을 따르고, omp용으로 다음을 바꾼다.
1. bun(1.3 이상), git, omp가 있는지 확인한다. omp 버전이 18.14.2보다 낮으면 업그레이드 방법을 안내하고 멈춘다(omp-ulw 기준선). herdr가 없으면 설치 방법을 안내한다. 원본처럼 herdr 설치는 setup 단계에서 사람이 진행한다.
2. omp-dori를 `${DORI_SRC:-~/.dori/src}`에 clone하거나 `pull --ff-only`로 갱신한다.
3. omp-ulw를 `${OMP_ULW_SRC:-~/.dori/omp-ulw}`에 clone하고, omp-dori가 고정한 커밋으로 checkout한다(4.9). 이미 있으면 그 커밋으로 맞춘다. 사용자가 `OMP_ULW_SRC`로 자기 사본을 지정하면 그것을 쓰고 checkout은 하지 않는다.
4. `skills/dori-mode`를 `${SKILLS_DIR:-~/.agents/skills}/dori-mode`에 심볼릭 링크한다(원본과 같은 규칙: 링크가 아닌 기존 폴더는 건드리지 않음).
5. `skills/dori-mode/scripts`에서 `bun install && bun link`를 실행해 `dori`를 PATH에 올린다.
6. omp-dori 확장을 `~/.omp/agent/extensions/`에 심볼릭 링크한다(4.2). 같은 이름의 링크가 아닌 파일이 있으면 건드리지 않고 멈춘다.
7. `~/.dori/config.json`이 없으면 예제를 복사하고 `ompUlwPath`를 채운다. 이미 있으면 `ompUlwPath`만 없을 때 추가한다.
8. 마지막에 다음 단계를 출력한다: `herdr integration install omp`, Dori 세션 시작 명령(`omp --dori-role dori`), config 편집, "Dori mode"라고 말하기.

제거 방법(링크 세 개와 `~/.dori/src`, `~/.dori/omp-ulw` 삭제)을 README에 적는다. 별도 uninstall 스크립트는 만들지 않는다(원본에도 없다).

### 4.9 배포

- 저장소: `https://github.com/betive37/omp-dori` (공개). install.sh의 clone 주소는 `https://github.com/betive37/omp-dori.git`이고, 설치 명령은 `curl -fsSL https://raw.githubusercontent.com/betive37/omp-dori/main/install.sh | bash`다.
- 라이선스는 원본과 같은 MIT이고, 원본 저작권 고지(Copyright 2026 YeonGyu Kim)를 유지한 채 omp-dori 고지를 덧붙인다.
- omp-ulw는 저장소에 넣지 않는다. 최상위 라이선스 파일이 없어서 재배포 조건이 불분명하기 때문이다(3절). install.sh가 원래 저장소(`github.com/dbc-hbin/omp-ulw`)에서 받는다. 받을 커밋은 omp-dori 저장소의 파일 하나(예: `omp-ulw.lock`, 커밋 SHA 한 줄)로 고정한다. 고정한 커밋을 올리는 일은 omp-dori 갱신으로 다룬다. omp-ulw의 기본 export 형태(`ompUlw(pi)`)가 바뀌면 omp-dori가 깨지므로 테스트로 고정한다(4.8).
- 원본 dori-superagent의 변경을 받아오기 위해 저장소에 `upstream` remote를 둔다. 원본 파일 경로(`skills/dori-mode/…`)를 그대로 유지해서 cherry-pick 충돌을 줄인다.
- README는 원본 구조를 따른다. 무엇인지, 원본과 무엇이 다른지(omp, omo 프롬프트, `monitor` 도구, `--dori-role`), 설치, 요구사항(omp 18.14.2 이상, bun 1.3 이상, herdr, herdr omp 통합), 평소 omp 세션에 남는 차이(4.2), 제거 방법을 담는다. [내 결정: 영어 `README.md`를 기본으로 하고 한국어 `README.ko.md`를 함께 둔다. 원본이 다국어 README를 둔 것을 따랐다.]
- 개발용 `ref/`는 배포 저장소에 넣지 않는다.

### 4.8 테스트와 검증

- 원본 `scripts/test/` 전체가 통과해야 한다. omo 전용 기대값(세션 폴더 인코딩, goal 문구)은 omp 기준으로 바꾼다. 원본의 가짜 실행기 구조(`test/fakes.ts`)를 그대로 쓴다.
- 새 테스트(동작이 바뀐 곳만): 세션 ID 확인 순서(확장 파일 → 프로세스와 시작 시각), omp 폴더 인코딩(홈/임시/절대), pause/resume 확인과 UNVERIFIED 경로, monitor 도구의 필터·묶음·프로세스 종료 알림·세션 종료 시 정리, 역할 기록과 복원(플래그로 시작 → 플래그 없이 재개해도 역할 유지, 역할 없는 세션에서는 omp-ulw의 이벤트 handler와 명령 handler가 실행되지 않고 `sendUserMessage`도 호출되지 않음, Dori 세션 → 평소 세션 → Dori 세션 전환 때 역할과 omp-ulw 상태가 세션마다 맞게 바뀜), `goal` 도구가 역할이 `lane`이고 goal이 없는 세션에서만 켜지는지(Dori 세션과 평소 세션에서는 켜지지 않음), 고정된 omp-ulw 커밋의 기본 export가 `ompUlw(pi)` 형태인지.
- 실제 환경 확인(테스트로 대신하지 않음):
  0. 설치 후 역할 없이 띄운 평소 omp 세션의 시스템 프롬프트와 활성 도구 목록이 설치 전과 같은지 비교한다. 다른 점이 있으면 4.2의 "남는 차이" 범위 안인지 확인한다. 같은 평소 세션에서 `/ulw 테스트`와 `ulw 테스트`를 입력해 프롬프트가 주입되지 않고 goal이 생기지 않는지 본다. Dori 세션에서 `/resume`으로 평소 세션으로 넘어간 뒤에도 같은 결과인지 본다.
  1. herdr pane에서 `omp --dori-role dori`로 Dori 세션을 띄운다. omo 프리셋이 들어갔는지, `monitor` 도구가 켜졌는지 확인한다. `monitor`로 `dori watch`를 걸고, 맞는 줄이 들어오면 세션이 깨어나는지 본다.
  2. `dori launch`로 실제 레인을 하나 띄운다. `STARTUP_OK`, 첫 턴에서 ULW가 켜지는지, 레인 에이전트가 `goal create`로 브리프에 맞는 objective를 직접 써서 goal을 만드는지, 레지스트리에 `via: "extension"` 세션 ID가 기록되는지 확인한다.
  3. `dori pause`와 `dori resume`이 `PAUSED`, `RESUMED`를 내고 goal 상태가 실제로 바뀌는지 본다.
  4. claim-done부터 watch 자동 종료까지 한 번 끝까지 돌린다(`Done = command [...]` 신호 사용).
  5. omp 승인 대화상자가 떴을 때 herdr omp 통합이 blocked로 보고하는지 확인한다. omp 상태는 통합 확장이 보고하므로 화면 감지 규칙으로는 보정할 수 없다. blocked로 보고하지 않으면 원본의 "승인 창에는 보내지 않는다" 규칙이 깨진다. 이때는 omp-dori 확장이 `tool_approval_requested` 이벤트(omp `types.ts:1685`)를 받아 상태 파일에 승인 대기를 기록하고, `sendVerified`가 그 기록이 있는 pane에는 보내지 않게 한다. [내 결정: 원본에 없는 코드지만 원본 규칙을 지키기 위한 것이라 범위 안으로 본다.]
  6. 레인을 닫았다가 `omp --resume=<세션>`으로 플래그 없이 다시 연다. omo 프리셋과 goal, 상태 파일 기록이 유지되는지 본다. herdr 서버 재시작 후 자동 복원도 같은 방식으로 확인한다.
  7. 깨끗한 계정이나 다른 머신에서 README의 한 줄 설치를 처음부터 끝까지 따라 해 본다.

## 5. 구현자에게 맡기는 것

- omp-dori 확장 내부 구조, 파일 분할, 묶음 기준, 상태 파일 쓰기 방식(원자적 쓰기 권장).
- 4.3에서 `goal` 도구를 켜는 정확한 시점(세션 시작, 첫 턴 직전 등). 첫 턴의 도구 목록에 `goal`이 들어 있어야 한다.
- 4.2에서 서브에이전트가 부모 역할을 알아내는 방법, 평소 세션에 omp-ulw 명령이 보이지 않게 하는 방법.
- `STARTUP_FAILURE`에 추가할 omp 문구.
- omp 폴더 인코딩 재현 방법. omp 패키지 내부를 import할지 규칙을 다시 구현할지는 구현자가 정하되, 다시 구현한다면 omp 18.14.2 동작과 맞는지 테스트로 고정한다.

## 6. 미해결 전제

- 이 맥에 herdr를 설치해야 한다.
- omp를 18.14.2 이상으로 올려야 한다(현재 18.12.2).
- 메신저 계정과 봇 설정은 원본 setup 절차대로 사용자가 진행한다.

