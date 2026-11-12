#!/usr/bin/env bash
# Installs the complete OMP Dori candidate: skill CLI, harness sources, extension, and pinned omp-ulw.
# DORI_NO_PULL=1 (or true, yes) installs an existing checkout without updating it.
# DORI_INSTALL_MODE=primary installs into ~/.dori; DORI_INSTALL_MODE=side installs next to another Dori, in
# OMP_DORI_HOME (default ~/.omp-dori), without changing ~/.dori, the dori command or the dori-mode skill.
# Without DORI_INSTALL_MODE, a ~/.dori owned by another Dori is never changed: a terminal is asked, anything else stops.
# Optional paths: DORI_SRC, OMP_ULW_SRC, SKILLS_DIR, OMP_EXTENSIONS_DIR, OMP_DORI_HOME, OMP_DORI_BIN_DIR.
set -euo pipefail

fail() { echo "$1" >&2; exit 1; }
absolute() { case "$1" in /*) printf '%s\n' "$1" ;; *) printf '%s/%s\n' "$PWD" "$1" ;; esac; }
at_least() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | { read -r first; printf '%s\n' "$first"; })" = "$1" ]; }

repo="https://github.com/vntrevx/omp-dori.git"
ulw_repo="https://github.com/dbc-hbin/omp-ulw.git"
primary_home="$HOME/.dori"
side_home="$(absolute "${OMP_DORI_HOME:-$HOME/.omp-dori}")"
min_bun="1.4.4"
min_omp="18.18.8"

command -v git >/dev/null || fail "git is required"
command -v bun >/dev/null || fail "bun $min_bun+ is required: https://bun.sh"
command -v omp >/dev/null || fail "omp $min_omp+ is required"
bun_version="$(bun --version)"
at_least "$min_bun" "$bun_version" || fail "bun $bun_version is older than $min_bun"
omp_version="$(omp --version)"
[[ "$omp_version" =~ ([0-9]+(\.[0-9]+){2}) ]] || fail "could not read omp version: $omp_version"
omp_version="${BASH_REMATCH[1]}"
at_least "$min_omp" "$omp_version" || fail "omp $omp_version is older than $min_omp"

# Every install of this repository leaves a marker in its home. Installs that predate it are known by the
# omp-dori checkout in ~/.dori/src or by the omp-dori extension link every earlier install.sh created.
marked() { [ -e "$1/.omp-dori" ]; }
earlier_extension_link="$(absolute "${OMP_EXTENSIONS_DIR:-$HOME/.omp/agent/extensions}")/omp-dori"
is_omp_dori() { case "$(git -C "$1" remote get-url origin 2>/dev/null)" in */omp-dori | */omp-dori.git) return 0 ;; esac; return 1; }
other_dori() {
  [ -d "$primary_home" ] && [ -n "$(ls -A "$primary_home")" ] || return 1
  marked "$primary_home" && return 1
  [ -d "$primary_home/src/.git" ] && is_omp_dori "$primary_home/src" && return 1
  [ -L "$earlier_extension_link" ] && return 1
  return 0
}
other=""
other_dori && other=1

mode="${DORI_INSTALL_MODE:-}"
case "$mode" in "" | primary | side) ;; *) fail "DORI_INSTALL_MODE=$mode: use primary or side" ;; esac
if [ -z "$mode" ]; then
  if marked "$side_home"; then
    mode=side
  elif [ -z "$other" ]; then
    mode=primary
  elif [ -t 0 ]; then
    cat >&2 <<EOF
$primary_home belongs to another Dori installation.
  1) primary  install omp-dori into $primary_home. It takes over the dori command, the dori-mode skill link
              and $primary_home/config.json; copies of the previous ones are kept under $primary_home/backups.
  2) side     install omp-dori into $side_home, next to it. $primary_home, the dori command and the
              dori-mode skill stay as they are; this installation is used through the omp-dori command.
EOF
    read -r -p "Install as 1 or 2 [2]: " answer
    case "${answer:-2}" in
      1 | primary) mode=primary ;;
      2 | side) mode=side ;;
      *) fail "unknown answer: $answer" ;;
    esac
  else
    fail "$primary_home belongs to another Dori installation, so nothing was changed. Run install.sh in a terminal to choose, or set DORI_INSTALL_MODE=side (install into $side_home next to it) or DORI_INSTALL_MODE=primary (take over $primary_home)."
  fi
fi

if [ "$mode" = side ]; then base="$side_home"; else base="$primary_home"; fi
src="$(absolute "${DORI_SRC:-$base/src}")"
ulw="$(absolute "${OMP_ULW_SRC:-$base/omp-ulw}")"
skills="$(absolute "${SKILLS_DIR:-$HOME/.agents/skills}")"
extensions="$(absolute "${OMP_EXTENSIONS_DIR:-$HOME/.omp/agent/extensions}")"
bin_dir="$(absolute "${OMP_DORI_BIN_DIR:-${BUN_INSTALL:-$HOME/.bun}/bin}")"

if [ -z "${DORI_SRC:-}" ] && [ -d "$src/.git" ] && ! is_omp_dori "$src"; then
  fail "$src is a checkout of $(git -C "$src" remote get-url origin 2>/dev/null || echo 'another repository'), not omp-dori; nothing was changed. Move it aside, set DORI_SRC, or install with DORI_INSTALL_MODE=side."
fi

if [ "$mode" = primary ] && [ -n "$other" ]; then
  backup="$primary_home/backups/omp-dori-install-$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$backup"
  [ -f "$primary_home/config.json" ] && cp -p "$primary_home/config.json" "$backup/config.json"
  for path in "$skills/dori-mode" "$bin_dir/dori"; do
    printf '%s -> %s\n' "$path" "$(readlink "$path" 2>/dev/null || echo '(not a link)')"
  done > "$backup/links.txt"
  echo "The previous config.json and link targets are kept in $backup" >&2
fi

if [ ! -d "$src/.git" ]; then
  git clone --quiet --depth 1 "$repo" "$src"
elif [[ "${DORI_NO_PULL:-}" =~ ^(1|true|yes)$ ]]; then
  echo "DORI_NO_PULL=$DORI_NO_PULL: installing $src as it is, without pulling" >&2
elif ! git -C "$src" fetch --quiet; then
  echo "Could not fetch updates for $src; git's message above says why." >&2
  exit 1
elif ! git -C "$src" merge --ff-only --quiet '@{u}'; then
  if git -C "$src" rev-parse --verify --quiet '@{u}' >/dev/null &&
    [ -n "$(git -C "$src" status --porcelain)$(git -C "$src" rev-list '@{u}..HEAD')" ]; then
    echo "Could not update $src. Local work in that checkout:" >&2
    git -C "$src" status --short --branch >&2
    git -C "$src" log --oneline '@{u}..HEAD' >&2
    echo "To keep it and install the checkout as it is, run install.sh again with DORI_NO_PULL=1." >&2
  else
    echo "Could not update $src; git's message above says why." >&2
  fi
  exit 1
fi

if [ -n "${OMP_ULW_SRC:-}" ]; then
  [ -f "$ulw/index.ts" ] || fail "OMP_ULW_SRC=$ulw has no index.ts"
else
  pin="$(tr -d '[:space:]' < "$src/omp-ulw.lock")"
  [ -d "$ulw/.git" ] || git clone --quiet "$ulw_repo" "$ulw"
  git -C "$ulw" fetch --quiet origin
  git -C "$ulw" -c advice.detachedHead=false checkout --quiet "$pin"
fi

link() {
  if [ -e "$2" ] && [ ! -L "$2" ]; then
    echo "$2 exists and is not a link; leaving it alone" >&2
    return 1
  fi
  ln -sfn "$1" "$2"
}

if [ "$mode" = primary ]; then
  mkdir -p "$skills" "$extensions"
  link "$src/skills/dori-mode" "$skills/dori-mode" || true
  link "$src/extension" "$extensions/omp-dori" || fail "extension link was not installed"
fi

# In primary mode the skill owns the installed dori name; a side install never links a global dori.
(cd "$src" && bun install --frozen-lockfile --silent)
(cd "$src/skills/dori-mode/scripts" && bun install --frozen-lockfile --silent)
[ "$mode" = primary ] && (cd "$src/skills/dori-mode/scripts" && bun link --silent)
(cd "$src/extension" && bun install --frozen-lockfile --silent)

mkdir -p "$base"
config="$base/config.json"
created=""
[ -f "$config" ] || { cp "$src/skills/dori-mode/references/config.example.json" "$config" && created=1; }
side_omp=""
[ "$mode" = side ] && side_omp="$base/bin/omp"
CONFIG="$config" ULW_INDEX="$ulw/index.ts" ULW_EXPLICIT="${OMP_ULW_SRC:+1}" CREATED="$created" SIDE_OMP="$side_omp" \
  STATE_DIR="$base/state" bun -e '
  const path = process.env.CONFIG, index = process.env.ULW_INDEX;
  const config = await Bun.file(path).json();
  let changed = false;
  if (process.env.CREATED && process.env.SIDE_OMP) {
    // A new side configuration keeps its state and lanes inside the side home, never in ~/.dori.
    const rest = Array.isArray(config.agentCommand) ? config.agentCommand.slice(1) : ["--dori-role", "lane", "--model", "{model}", "{prompt}"];
    config.stateDir = process.env.STATE_DIR;
    config.agentCommand = [process.env.SIDE_OMP, ...rest];
    changed = true;
  }
  if (process.env.ULW_EXPLICIT || !config.ompUlwPath || config.ompUlwPath === "~/.dori/omp-ulw/index.ts") {
    config.ompUlwPath = index;
    changed = true;
  }
  if (changed) await Bun.write(path, JSON.stringify(config, null, 2) + "\n");'
printf '%s\n' "$mode" > "$base/.omp-dori"

command -v herdr >/dev/null || command -v hc >/dev/null || echo "Install herdr or hc before launching sessions." >&2

if [ "$mode" = primary ]; then
  cat <<EOF
Installed.
  Skill:     $skills/dori-mode
  Extension: $extensions/omp-dori
  omp-ulw:   $ulw
  CLI:       dori (messenger/lane CLI), dori harness (upstream harness)
  Config:    $config
Next: configure the runtime and hooks, install the omp herdr integration if using herdr,
then start omp --dori-role dori. Installation does not start a messenger or watcher.
EOF
  exit 0
fi

real_omp() {
  local dir
  local IFS=:
  for dir in $PATH; do
    [ -n "$dir" ] && [ "$dir" != "$base/bin" ] && [ -f "$dir/omp" ] && [ -x "$dir/omp" ] && { printf '%s\n' "$dir/omp"; return 0; }
  done
  fail "omp is not on PATH"
}
hc_session="${OMP_DORI_HC_SESSION:-omp-dori}"
omp_bin="$(real_omp)"
bun_bin="$(command -v bun)"
skill="$base/skills/omp-dori-mode"
mkdir -p "$base/bin" "$base/state" "$base/skills"
rm -rf "$skill"
mkdir -p "$skill"
# omp lists one of two skills with the same description, so the copy changes its description as well as its name.
awk 'NR == 1 { print; next }
  !done && /^name: dori-mode$/ { print "name: omp-dori-mode"; next }
  !done && /^description: / { sub(/^description: /, "description: For the omp-dori installation next to another Dori (omp-dori command). "); print; next }
  !done && /^---$/ { done = 1 }
  { print }' "$src/skills/dori-mode/SKILL.md" > "$skill/SKILL.md"
for part in references scripts assets; do
  [ -e "$src/skills/dori-mode/$part" ] && ln -sfn "$src/skills/dori-mode/$part" "$skill/$part"
done
quoted_skills="$(bun -e 'console.log(JSON.stringify(process.argv[1]))' "$base/skills")"
cat > "$base/omp.yml" <<EOF
# Generated by install.sh for omp-dori omp sessions; rewritten on each install.
skills:
  ignoredSkills:
    - dori-mode
  customDirectories:
    - $quoted_skills
EOF
{
  printf 'export DORI_HOME=%q\n' "$base"
  printf 'export DORI_CONFIG=%q\n' "$config"
  printf 'export DORI_STATE_DIR=%q\n' "$base/state"
  printf 'export DORI_AGENT_CMD=%q\n' "$base/bin/omp"
  printf 'export HC_SESSION=%q\n' "$hc_session"
  printf 'case ":$PATH:" in *:%q:*) ;; *) export PATH=%q:"$PATH" ;; esac\n' "$base/bin" "$base/bin"
} > "$base/env.sh"
wrapper() {
  { printf '#!/usr/bin/env bash\n. %q\n' "$base/env.sh"; cat; } > "$base/bin/$1"
  chmod 755 "$base/bin/$1"
}
printf 'exec %q %q "$@"\n' "$bun_bin" "$src/skills/dori-mode/scripts/src/cli.ts" | wrapper dori
printf 'exec %q --no-extensions -e %q --config %q "$@"\n' "$omp_bin" "$src/extension" "$base/omp.yml" | wrapper omp
printf 'case "${1:-}" in\n  omp) shift; exec %q "$@" ;;\n  hc) shift; exec hc "$@" ;;\n  *) exec %q "$@" ;;\nesac\n' \
  "$base/bin/omp" "$base/bin/dori" | wrapper omp-dori
mkdir -p "$bin_dir"
link "$base/bin/omp-dori" "$bin_dir/omp-dori" || echo "Run $base/bin/omp-dori directly." >&2

cat <<EOF
Installed next to the Dori in $primary_home, which was not changed.
  Home:       $base (config.json, state, generated skill, omp.yml and wrappers)
  Command:    $bin_dir/omp-dori  (omp-dori <dori command> | omp-dori omp ... | omp-dori hc ...)
  Skill:      omp-dori-mode; dori-mode is hidden in omp-dori omp sessions
  Extension:  $src/extension, loaded only by omp-dori omp, which loads no other OMP extension
  omp-ulw:    $ulw
  hc session: $hc_session
  Config:     $config
Next: create a separate messenger bot and channel, set runtime, leadPane, defaultCwd, defaultModel and hooks
in $config, then start the lead in its own hc session:
  omp-dori hc run --name omp-dori-lead -- omp-dori omp --dori-role dori
Installation does not start a messenger, watcher or hc server.
EOF
