# 워커 수동 점검표 (실제 PC 두 대)

CI는 가짜 `ssh`로 워커를 시험합니다. 진짜 sshd, Tailscale, 재부팅, 네트워크 전환은 여기서 손으로 확인합니다.
A는 같은 네트워크의 일반 SSH, B는 Tailscale(Tailscale SSH가 켜진 Linux 워커)입니다. 예시 값은 바꿔 쓰세요:
워커 사용자 `dev`, 워커 IP `192.168.0.20`, tailnet 이름 `linux-box`, 저장소 `github.com/OWNER/REPO`.

## 0. 준비

- 허브 PC: 이 브랜치의 `cataly`, 디스코드 연결, `cataly status` 정상.
- 워커 PC, 레인을 돌릴 사용자(`dev`, 매일 쓰는 계정이 아니면 좋음)로 로그인해서:
  - `curl -fsSL https://raw.githubusercontent.com/vntrevx/Cataly/main/install.sh | sh` (이 브랜치 빌드면 그 바이너리를 `~/.local/bin/cataly`에)
  - OMP 설치 후 로그인 한 번, `omp --version` 확인
  - `git ls-remote https://github.com/OWNER/REPO` 성공
- 워커 sshd 켜기: Linux `sudo systemctl enable --now ssh` (Fedora는 `sshd`), macOS `sudo systemsetup -setremotelogin on`.
- 저장소의 시험 커밋은 origin에 푸시해 둡니다. 커밋 안 된 수정은 워커로 가지 않습니다.

## A. 일반 SSH

1. 허브: `cataly worker add dev@192.168.0.20 --name linux-box`
   → `cataly worker init --hub-key '…'` 줄이 나옵니다.
2. 워커(`dev`): 그 줄을 그대로 실행.
   → 호스트 키 지문과 허브에서 칠 `cataly worker add … --host-key SHA256:…` 줄이 나옵니다.
   - `ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub`의 지문과 같은지 확인.
   - `grep cataly-hub ~/.ssh/authorized_keys` → `restrict,command="…cataly worker serve --hub …"`로 시작.
3. 허브: `cataly worker add dev@192.168.0.20 --name linux-box --host-key SHA256:… --repo github.com/OWNER/REPO`
   → `linux-box enrolled · Linux x64 · cataly … (compatible) · …`.
4. 허브: `cataly worker list`, `cataly worker doctor linux-box` → 모두 통과.
5. 경계 확인(허브):
   `ssh -F none -T -i "$HOME/.config/cataly/secrets/worker-linux-box" -o IdentitiesOnly=yes -o UserKnownHostsFile="$HOME/.config/cataly/known_hosts" dev@192.168.0.20 'echo SHELL-OPEN' </dev/null`
   → `SHELL-OPEN`이 **나오면 실패**(강제 명령이 적용되지 않음).
6. 원격 레인: 디스코드에서 "linux-box에서 REPO 테스트 돌려줘".
   - 스레드 첫 글 `🖥️ linux-box (Linux x64) · <커밋>`, 완료 체크에 `(linux-box)`.
   - 운영 패널 `🛰️ 워커: linux-box 🟢 레인 1/1`. 원격 레인 비용은 "알 수 없음"이 정상입니다.
   - 워커: `ls ~/.local/state/cataly/worker/*/lanes/` 에 레인 작업 폴더.
7. 연결 끊김: 레인이 일하는 중에 워커 네트워크를 끕니다(Wi-Fi 끄기 또는 랜선 뽑기).
   - 약 45초 안에 스레드 `작업 PC linux-box 연결이 끊겼어요. 다시 연결되면 이어서 해요`.
   - 패널 `linux-box 🟡 다시 연결 중 · 레인 1/1`.
   - 네트워크를 켜면 1분 안에 `다시 연결돼서 이어서 해요`, 패널 🟢, 같은 세션에서 이어 감.
8. 전달 중 끊김: 스레드에 메시지를 보내자마자 워커 네트워크를 끕니다. 다시 연결된 뒤
   그 메시지는 레인 세션에 한 번 있거나, `cataly events --since 10m | grep 'in doubt'`에
   `in doubt, not resent`로 나옵니다. 같은 메시지가 레인에 **두 번 들어가면 실패**.
   in doubt면 리드가 확인하고 풀 때까지(`lane.release`) 리드의 `lane.send`가 막힙니다.
9. 강제 종료: 워커에서 `pkill -KILL -f 'cataly worker serve'` → 7과 같이 이어 감.
10. 재부팅: 워커 `sudo reboot` → 워커에 Cataly 서비스 없이 sshd만 뜨면 레인이 이어 감.
11. (선택, 워커의 SSH 신원이 바뀜) 호스트 키 변경:
    `sudo ssh-keygen -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key` (덮어쓰기 `y`), `sudo systemctl restart ssh`
    → 허브 `cataly worker doctor linux-box`가 `refused: host key changed…`, 패널 `⛔ 연결 거부`. 자동으로 받아들이면 실패.
    복구: 1~3단계를 다시(같은 키가 다시 쓰이고 지문만 새로 고정).
12. 정리: 레인을 닫고 허브 `cataly worker remove linux-box`
    → 워커 `grep cataly-hub ~/.ssh/authorized_keys`에 아무것도 없음.

## B. Tailscale

두 PC가 같은 tailnet에 있고 `tailscale status`에서 서로 보여야 합니다. 허브 `tailscale ip -4 linux-box` → `100.x.y.z`.

1. 워커에서 Tailscale SSH 켜기: `sudo tailscale set --ssh`. 이제 tailnet의 22번은 Tailscale이 받습니다.
2. 워커 OpenSSH를 tailnet 주소의 2222번에도 엽니다(ListenAddress를 하나라도 쓰면 적힌 주소만 들으므로 22도 적음):
   ```sh
   TSIP=$(tailscale ip -4)
   printf 'ListenAddress 0.0.0.0:22\nListenAddress [::]:22\nListenAddress %s:2222\n' "$TSIP" \
     | sudo tee /etc/ssh/sshd_config.d/50-cataly-tailnet.conf
   sudo mkdir -p /etc/systemd/system/ssh.service.d
   printf '[Unit]\nAfter=tailscaled.service\nWants=tailscaled.service\n' \
     | sudo tee /etc/systemd/system/ssh.service.d/after-tailscale.conf
   sudo systemctl daemon-reload && sudo systemctl restart ssh
   ss -ltn | grep 2222
   ```
   Ubuntu 24.04처럼 `ssh.socket`을 쓰면 먼저 `sudo systemctl disable --now ssh.socket && sudo systemctl enable --now ssh.service`.
3. 거부 확인(22번, Tailscale SSH): 허브 `cataly worker add dev@linux-box --tailscale --name ts22`, 워커에서 나온 init 줄
   실행, 허브에서 `cataly worker add dev@linux-box --tailscale --name ts22 --host-key SHA256:…`
   → 지문 불일치 또는 `no forced command`로 **등록 거부**되어야 합니다.
   정리: 워커에서 같은 init 줄 끝에 `--remove`를 붙여 실행, 허브 `rm ~/.config/cataly/secrets/worker-ts22*`.
4. 허브: `cataly worker add dev@linux-box:2222 --tailscale --name linux-box`
   → init 줄에 `--from 100.…`(허브의 tailnet 주소).
5. 워커: 그 줄 실행 → `authorized_keys` 줄에 `from="100.…"`, sshd를 tailscaled 뒤에 띄우라는 안내.
6. 허브: `cataly worker add dev@linux-box:2222 --tailscale --name linux-box --host-key SHA256:… --repo github.com/OWNER/REPO`
   → `grep 2222 ~/.config/cataly/known_hosts`에 `[100.x.y.z]:2222`.
7. tailnet 밖 주소 거부: 허브 `cataly worker add dev@example.com --tailscale` → `example.com is not on the tailnet…; nothing was enrolled`.
8. 허브 `cataly worker doctor linux-box` → tailscale 주소 확인까지 통과.
9. A의 6~10을 반복합니다. 추가로 워커 노트북의 네트워크를 바꿔 봅니다(Wi-Fi → 휴대폰 핫스팟):
   tailnet IP가 그대로라 45초 안이면 끊기지 않거나, 끊겨도 1분 안에 이어 가야 합니다.
10. 재부팅 뒤 `ss -ltn | grep 2222`에 `100.x.y.z:2222`가 보이고 레인이 이어 가야 합니다.

## 기록

| 항목 | A | B | 메모 |
| --- | --- | --- | --- |
| 등록과 고정 (A1-4, B4-6) | | | |
| 강제 명령 경계 (A5, B3) | | | |
| 원격 레인과 체크 (A6) | | | |
| 끊김·재연결·패널 (A7, A9) | | | |
| 전달 중 끊김, 재전송 없음 (A8) | | | |
| 재부팅 (A10, B10) | | | |
| 호스트 키 변경 거부 (A11) | | | |
| tailnet 밖 거부, 네트워크 전환 (B7, B9) | | | |
| 제거 (A12) | | | |
