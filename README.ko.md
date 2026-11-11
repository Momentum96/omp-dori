<p align="center">
  <img src="skills/dori-mode/assets/dori-avatar.png" alt="Dori" width="120">
</p>

# omp-dori

[English](README.md) | 한국어

Dori는 Slack, Discord, Telegram에서 요청을 받아 일마다 별도의 코딩 세션을 열고, 완료 증거를 확인한 뒤 같은 대화에 보고하는 상시 엔지니어입니다. 이 저장소는 [sisyphuslabs/dori-superagent](https://github.com/sisyphuslabs/dori-superagent/tree/97d807b9037cb050a72129659cf6306d8055010d)의 전체 공개 소스를 OMP로 포트한 수정본입니다. 기준은 upstream `97d807b9037cb050a72129659cf6306d8055010d`, 기존 OMP 소스 `75a1b28de8caddb6d30b2e3d72ef3dec09450b47`입니다.

**실행은 설치자가 명시적으로 시작합니다.** 최신 upstream의 harness, 의존성, 스킬과 OMP 확장을 함께 포함합니다. 설치 스크립트는 코드·의존성·링크를 준비할 뿐 messenger, watcher, harness를 시작하거나 기존 운영 Dori를 교체하지 않습니다. runtime·모델·메신저 설정 후 세션을 시작하세요. 업데이트 후보의 검증과 실제 활성화도 별도 단계입니다.

## 보존한 기능

| 영역 | 동작 |
|---|---|
| 요청과 보고 | 작은 질문·조회는 Dori가 처리하고, 긴 작업·병렬 작업·PR은 레인에 맡깁니다. lead만 사람에게 보고하며 같은 스레드의 상태 메시지를 수정하고 15분마다 또는 이정표마다 진척을 남깁니다. |
| 메신저 | Slack 스레드, Discord 스레드 생성·이름 변경·보관과 typing, Telegram forum topic 생성·이름 변경·닫기·재개와 typing/draft streaming을 유지합니다. Slack/Telegram inbound, Slack/Discord presence, 메시지 게시·수정, 설정한 음성 전사 명령도 포함합니다. 토큰은 실행 시 설정합니다. Discord 수신은 메신저 watch 도구를 사용합니다. |
| 스킬 레인 | launch/adopt/sync, 호스트의 메모리·디스크·세션 수 가드, heavy slot, 진행 보고, `Done =` 신호와 claim-done 검증을 유지합니다. 신호는 PR merge, issue close, npm version, argv 명령, 파일/hash/JSON, URL 응답을 지원하며 모두 통과해야 닫힙니다. |
| 영속 기록 | 레지스트리, reports, 요청 worklog, 재개용 checkpoints, 저장소별·전역 lessons, 예산·정체 감시와 재시작 후 `dori boot` 점검을 유지합니다. boot는 읽기 점검입니다. |
| 전체 harness | lead/duty 세션, duty loop, hourly loop, lane, inbox와 모호한 전송 수동 정리, backlog와 GitHub issue import, 의사결정 알림, 두 runtime 간 migration을 보존합니다. |
| review/merge | 정확한 PR head의 새 OMP reviewer가 남긴 CLEAR receipt와 하나 이상의 통과한 check가 필요합니다. head가 바뀌면 재리뷰하며, merge는 head를 고정해 squash합니다. |
| hooks/관측 | harness onReady/onReport/onLost/reviewCommand, 스킬 threadReply/threadDone/transcribe, prompt override와 upstream telemetry를 유지합니다. `DORI_TELEMETRY=0` 또는 `DO_NOT_TRACK=1`로 telemetry를 끕니다. |

각 메신저의 토큰·파일·수신 범위, backlog 정렬·재시도·잠금, inbox gate, signal 문법, telemetry 필드는 [영문 README](README.md)에 빠짐없이 남아 있습니다. [시간 단위 loop](docs/hourly-loop.md), [runtime](docs/runtime.md), [container](docs/container.md) 문서도 함께 보세요. 그림과 다른 언어 upstream 번역은 원본 설명 자료로 보존했으며, 현 OMP 명령은 영문·한국어 문서를 기준으로 합니다.

## 명령 소유권

설치되는 기본 `dori`는 계속 스킬 CLI입니다. 모든 root harness 명령은 `dori harness <args>`로 전달합니다. root package를 별도로 `bun link`하면 `dori-harness`만 등록되며 기본 `dori`와 충돌하지 않습니다. checkout에서는 `bun src/cli.ts <args>`로도 전체 harness를 실행합니다.

| 스킬 CLI | harness CLI |
|---|---|
| `dori launch`, `claim-done`, `close`, `pause`, `resume` | `dori harness start`, `ready`, `done` |
| `dori send discord --to ... --text ...`는 메신저에 게시 | `dori harness send lead --text ...`는 에이전트 세션에 전달 |
| `dori report --kind ...`와 스킬 레지스트리 | `dori harness report --status ...`와 harness 레지스트리 |
| `dori boot`, `watch`, `freshness`, `guard`, `heavy` | `dori harness up`, `duty`, `loop`, `backlog`, `review`, `merge`, `migrate`, `inbox`, `status` |

`dori`는 스킬 도움말, `dori harness --help`와 `bun src/cli.ts --help`는 전체 root 도움말을 출력합니다. 스킬의 claim-done/close와 harness의 READY/done은 서로 다른 레지스트리와 완료 프로토콜입니다.

## OMP와 맞물리는 방식

lead와 duty는 `omp --dori-role dori`, 구현 레인과 기본 reviewer는 `omp --dori-role lane`으로 시작합니다. 확장은 역할을 세션 branch의 `omp-dori:role`에 저장하고 resume·세션 전환 때 복원합니다. 서브에이전트는 부모 역할을 이어받습니다. 플래그 없는 일반 세션에서는 Dori handler나 추가 goal/monitor가 활성화되지 않습니다.

- `ompUlwPath`의 [omp-ulw](https://github.com/dbc-hbin/omp-ulw)를 역할 세션에만 적용합니다. 현재 public pin은 [`omp-ulw.lock`](omp-ulw.lock)의 `cbfe5ee72e5b2783af01616e7f4d3da5e3f9180d`입니다. 명령·도구 이름은 확장 로드 시 등록되므로 일반 세션 목록에도 보일 수 있지만 실행 handler는 역할로 제한됩니다.
- `<stateDir>/sessions/<encodeURIComponent(id)>.json`에 pane/terminal ID, 역할, 세션 ID/파일, cwd, writer pid, 시각, goal, `approvalPending`과 선택 필드 `agentStatus`(idle/working)를 씁니다. ID는 `HC_TERMINAL_ID`, 그다음 `HERDR_PANE_ID`입니다. runtime은 현재 terminal의 살아 있는 OMP 프로세스·foreground·시작 시각과 기록을 대조합니다. 오래되거나 다른 프로세스의 기록을 신뢰하지 않으며 승인 창에는 force 전송도 입력하지 않습니다.
- hc 역할 세션에서는 확장이 native OSC 7501에 `app=omp`를 넣어 보고합니다. 세션 시작·agent_end는 idle, agent_start는 working, 열린 승인은 blocked입니다. 자신이 쓴 역할 기록을 해제하면 보고도 unknown으로 무효화합니다. runtime의 unknown은 계속 보류하며 hc의 `--if` gate도 유지합니다. 파일 기록만으로 native status를 대체하지 않습니다.
- goal 없는 레인에는 OMP의 native `goal` 도구를 켭니다. 브리프를 읽고 goal을 만드는 일은 레인의 몫입니다. active/paused/complete/dropped를 구별하고, budget-limited와 알 수 없는 상태는 blocked로 유지합니다. `dori pause`/`resume`은 native goal을 읽어 확인하며, 확인 실패는 exit 3과 unverified 결과로 남깁니다. OMP 재개 시 goal이 paused일 수 있으므로 기록을 확인하고 `/goal resume`으로 재개합니다.
- 역할 `dori`에는 기존 `monitor` 도구를 켭니다. 실행 argv/filter/cwd는 그 세션 branch와 `<stateDir>/monitors/<encodeURIComponent(sessionId)>.json`에 `{version:1,sessionId,monitors:[{name,argv,filter,cwd}]}`로 남겨 `dori boot`가 private transcript 없이 읽기 점검합니다. 해당 Dori 세션을 재개하면 기록된 monitor가 복원되고, 다른 세션으로 전환하면 그 실행 monitor가 멈춥니다. 새 설치나 container 시작이 수신기·watcher를 새로 만드는 것은 아닙니다. 이전 운영 세션·볼륨을 재사용하면 그 세션의 monitor가 복원될 수 있습니다.

역할 세션을 처음 사용한 뒤에는 그 OMP 프로세스에 승인 event hook이 유지됩니다. 그동안 OMP의 read speculation 설정 변화도 프로세스가 끝날 때까지 남습니다. 역할 세션을 사용한 적 없는 프로세스에는 이 hook을 등록하지 않습니다.

## runtime과 설정

herdr와 hc를 upstream의 `SessionRuntime`/`SessionRef`/`Runtimes`로 함께 지원합니다. 기존 레인·역할은 자신이 열린 runtime으로 명령을 받고, 새 세션 선택 순서는 `DORI_RUNTIME`, config의 `runtime`, 현재 hc terminal, 현재 herdr pane, PATH의 hc, PATH의 herdr입니다. herdr에서는 `herdr integration install omp`가 필요합니다. hc에서는 retained 확장이 역할 세션의 native OSC 7501을 발행하고, unknown은 계속 보류합니다. 자세한 verb/gate/마이그레이션 계약은 [runtime 문서](docs/runtime.md)에 있습니다.

스킬 config는 기본 `~/.dori/config.json`, 또는 `DORI_CONFIG`입니다. `DORI_STATE_DIR`은 `stateDir`보다, `DORI_LEAD_PANE`은 `leadPane`보다 우선합니다. 기본값은 다음과 같고 명시적인 사용자 설정을 보존합니다.

```json
{
  "agentCommand": ["omp", "--dori-role", "lane", "--model", "{model}", "{prompt}"],
  "defaultModel": "anthropic/claude-opus-5-5",
  "sessionsDir": "~/.omp/agent/sessions",
  "ompUlwPath": "~/.dori/omp-ulw/index.ts"
}
```

`dori launch --model`이 `defaultModel`보다 우선합니다. harness는 별도로 `DORI_AGENT_CMD=omp`가 기본이며, OMP 명령에 역할이 명시되지 않았을 때만 역할을 붙입니다. 명시된 command/role/model 인수는 유지하며 모델 인수가 없으면 OMP 자체 설정을 따릅니다. `dori harness review --model`도 유지합니다. Linux의 세션 경로 인코딩과 process 기반 ID 탐색도 보존합니다.

harness 기록은 `DORI_HOME`(기본 `~/.dori`), 스킬 장부·확장 기록은 `stateDir`(기본 `~/.dori/state`)에 둡니다. config의 `runtime`과 hook을 함께 쓰더라도 두 레지스트리는 별개입니다.

## 설치와 실행

Bun 1.4.4 이상, native OMP 18.18.8 이상, git, `gh`, herdr 또는 hc가 필요합니다. 새 설치는 다음과 같습니다.

```sh
git clone https://github.com/vntrevx/omp-dori.git "$HOME/.dori/src"
bash "$HOME/.dori/src/install.sh"
```

기존 설치는 디렉터리 위로 다시 clone하지 말고 그 설치의 `install.sh`를 실행하세요. 스크립트는 checkout을 fast-forward로만 갱신하고 root/skill/extension의 잠긴 의존성을 설치합니다. 스킬은 `$HOME/.agents/skills/dori-mode`, 확장은 `$HOME/.omp/agent/extensions/omp-dori`에 연결하며 스킬의 `dori`만 link합니다. public omp-ulw는 lock 커밋으로 받습니다. 소스 위치는 `DORI_SRC`, 직접 받은 omp-ulw는 `OMP_ULW_SRC`, 링크 위치는 `SKILLS_DIR`/`OMP_EXTENSIONS_DIR`로 지정합니다. config가 없을 때만 예제를 복사하며 기존 값은 유지합니다. 현재 checkout을 pull 없이 설치하려면 `DORI_NO_PULL=1 DORI_SRC="$PWD" bash install.sh`를 쓰세요. 격리 검증은 `HOME`, `DORI_HOME`, `DORI_CONFIG`, `DORI_STATE_DIR`을 함께 분리해야 합니다. sibling root runtime import 때문에 스킬만 떼어 복사하지 말고 전체 checkout을 유지하세요.

`runtime`, `leadPane`, `laneWorkspace`, `defaultCwd`, provider를 포함한 `defaultModel`과 메신저 hook을 설치 환경에 맞춥니다. herdr일 때 OMP integration을 설치하고, 선택한 runtime 안에서 `omp --dori-role dori`를 시작해 "Dori mode"라고 말하면 됩니다. 봇·메신저·읽기 온보딩은 [setup](skills/dori-mode/references/setup.md)이 안내합니다. 설치 스크립트는 messenger/presence/watcher/harness를 시작하지 않습니다.

전체 harness를 별도로 평가할 때는 선택한 runtime server를 시작한 뒤 아래 진입점을 사용합니다. `up`은 실제 lead/duty 및 두 loop를 시작합니다.

```sh
dori harness up
dori harness start fix-flaky-login --brief examples/brief.md
dori harness status
```

[`examples/container/`](examples/container/)는 현재 checkout을 build context로 사용하는 container 예제입니다. OMP와 확장, pinned omp-ulw를 포함하며, `.dori`/`.omp`/GitHub·messenger 인증/프로젝트를 격리 named volume에 보존하도록 구성합니다. 검증된 설치 경로는 native 설치 스크립트이며, container 예제는 게시되거나 독립 검증된 이미지가 아닙니다. 기본 시작은 runtime server와 idle 명령뿐입니다. [container 문서](docs/container.md)에 버전·checksum 출처, herdr/hc build 선택과 acceptance 절차가 있습니다.

## 개발과 검증 상태

```sh
bun install --frozen-lockfile
(cd skills/dori-mode/scripts && bun install --frozen-lockfile)
(cd extension && bun install --frozen-lockfile)
bun run ci
```

검사 결과는 해당 커밋의 CI와 실행 증거를 기준으로 확인하세요. fake runtime 기반 회귀 검사, 실제 herdr/hc의 격리 E2E, native OMP 세션과 실제 메신저 수신·응답은 서로 다른 증거입니다. 기본 suite에서 runtime 바이너리 부재로 건너뛴 E2E를 실제 실행 성공으로 간주하면 안 됩니다. 설치나 CI가 운영 전환 또는 두 번째 운영 gateway 실행을 뜻하지 않습니다.

기존 설치를 정리할 때는 링크와 설치 소스, `~/.dori`의 config/state, `~/.omp`의 세션·인증을 구분하세요. 설치는 기존 운영 세션이나 아카이브를 자동으로 지우지 않습니다.

## 라이선스와 출처

현재 upstream 코드는 [Sustainable Use License](LICENSE) 1.0이며, 26-29행은 자체 내부 업무 이용과 비상업·개인 이용을 허용하고 배포·고지 조건을 정합니다. 수정본 고지는 [NOTICE](NOTICE)에, 기존 OMP 통합 소스의 원래 고지는 [LICENSE.omp-dori](LICENSE.omp-dori)에 보존합니다. 현재 upstream 코드를 MIT로 바꾸는 것은 아닙니다. `ba96abf` 이전 upstream 커밋은 MIT였고, 별도로 받는 omp-ulw와 제3자 구성 요소의 고지는 각각 유지됩니다.
