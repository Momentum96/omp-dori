#!/usr/bin/env bash
# Installs the complete OMP Dori candidate: skill CLI, harness sources, extension, and pinned omp-ulw.
# DORI_NO_PULL=1 (or true, yes) installs an existing checkout without updating it.
# DORI_INSTALL_MODE=primary installs into ~/.dori; DORI_INSTALL_MODE=side installs next to another Dori, in
# OMP_DORI_HOME (default ~/.omp-dori), without changing ~/.dori, the dori command or the dori-mode skill.
# Without DORI_INSTALL_MODE, a ~/.dori owned by another Dori is never changed: a terminal is asked, anything else stops.
# omp-ulw follows OMP_ULW_REF (default main, so each run moves to the latest omp-ulw; lock uses omp-ulw.lock).
# omp-ulw is an OMP extension: a primary install links it into OMP_EXTENSIONS_DIR (or reuses a standalone omp-ulw
# there), a side install loads it through omp-dori omp; the omp-dori extension never imports a second copy.
# Optional paths: DORI_SRC, OMP_ULW_SRC, OMP_ULW_REPO, SKILLS_DIR, OMP_EXTENSIONS_DIR, OMP_DORI_HOME, OMP_DORI_BIN_DIR.
set -euo pipefail

fail() { echo "$1" >&2; exit 1; }
absolute() { case "$1" in /*) printf '%s\n' "$1" ;; *) printf '%s/%s\n' "$PWD" "$1" ;; esac; }
at_least() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | { read -r first; printf '%s\n' "$first"; })" = "$1" ]; }

repo="https://github.com/vntrevx/omp-dori.git"
ulw_repo="${OMP_ULW_REPO:-https://github.com/dbc-hbin/omp-ulw.git}"
ulw_ref="${OMP_ULW_REF:-main}"
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
# A checkout is omp-dori by its files, whatever its remote is named; other Doris ship no omp-ulw.lock.
marked() { [ -e "$1/.omp-dori" ]; }
earlier_extension_link="$(absolute "${OMP_EXTENSIONS_DIR:-$HOME/.omp/agent/extensions}")/omp-dori"
is_omp_dori() { [ -f "$1/omp-ulw.lock" ] && [ -f "$1/skills/dori-mode/scripts/src/cli.ts" ]; }
other_dori() {
  marked "$primary_home" && return 1
  [ -L "$earlier_extension_link" ] && return 1
  [ -d "$primary_home/src/.git" ] && is_omp_dori "$primary_home/src" && return 1
  [ -f "$primary_home/config.json" ] && return 0
  [ -d "$primary_home/src/.git" ] && return 0
  return 1
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
    read -r -p "Install as 1 or 2 [2]: " answer || fail "no answer; nothing was changed"
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

if [ "$mode" = primary ] && [ -n "$other" ]; then
  backup="$primary_home/backups/omp-dori-install-$(date +%Y%m%d-%H%M%S)"
  mkdir -p "$backup"
  [ -f "$primary_home/config.json" ] && cp -p "$primary_home/config.json" "$backup/config.json"
  for path in "$skills/dori-mode" "$bin_dir/dori" "$(command -v dori || echo "$bin_dir/dori")"; do
    printf '%s -> %s\n' "$path" "$(readlink "$path" 2>/dev/null || echo '(not a link)')"
  done | sort -u > "$backup/links.txt"
  echo "The previous config.json and link targets are kept in $backup" >&2
fi

standalone_ulw=""
if [ "$mode" = primary ] && [ -z "${OMP_ULW_SRC:-}" ]; then
  for entry in "$extensions"/*; do
    if [ "$entry" != "$extensions/omp-dori" ] && [ "$(readlink "$entry")" != "$ulw" ] &&
      [ -f "$entry/package.json" ] && grep -q '"name": *"omp-ulw"' "$entry/package.json"; then
      standalone_ulw="$entry"
      break
    fi
  done
fi

if [ -n "${OMP_ULW_SRC:-}" ]; then
  [ -f "$ulw/index.ts" ] || fail "OMP_ULW_SRC=$ulw has no index.ts"
elif [ -n "$standalone_ulw" ]; then
  echo "omp-ulw: using the standalone extension at $standalone_ulw; omp-dori does not load a second copy." >&2
else
  [ -d "$ulw/.git" ] || git clone --quiet "$ulw_repo" "$ulw"
  git -C "$ulw" fetch --quiet origin
  if [ "$ulw_ref" = lock ]; then
    target="$(tr -d '[:space:]' < "$src/omp-ulw.lock")"
  elif git -C "$ulw" rev-parse --verify --quiet "origin/$ulw_ref^{commit}" >/dev/null; then
    target="origin/$ulw_ref"
  else
    target="$ulw_ref"
  fi
  git -C "$ulw" -c advice.detachedHead=false checkout --quiet "$target"
  echo "omp-ulw: $ulw_ref at $(git -C "$ulw" rev-parse --short HEAD)" >&2
  # omp-ulw generates its frontend references from pinned submodules; without them only that skill lacks references.
  if [ -f "$ulw/upstream/materialize-frontend-refs.mjs" ]; then
    { git -C "$ulw" submodule update --init --recursive --quiet && (cd "$ulw" && bun upstream/materialize-frontend-refs.mjs >/dev/null); } ||
      echo "omp-ulw: frontend references were not generated; see $ulw/README.md" >&2
  fi
fi
# omp-ulw loads as its own OMP extension where one serves it: OMP resolves its host imports only there.
if [ "$mode" = side ]; then
  ulw_extension="$ulw"
elif [ -n "$standalone_ulw" ]; then
  ulw_extension="$standalone_ulw"
else
  ulw_extension="$extensions/omp-ulw"
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
  if [ -z "$standalone_ulw" ]; then
    link "$ulw" "$extensions/omp-ulw" || fail "omp-ulw extension link was not installed"
  fi
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
CONFIG="$config" CREATED="$created" SIDE_OMP="$side_omp" STATE_DIR="$base/state" bun -e '
  const path = process.env.CONFIG;
  const config = await Bun.file(path).json();
  let changed = false;
  if (process.env.CREATED && process.env.SIDE_OMP) {
    // A new side configuration keeps its state and lanes inside the side home, never in ~/.dori.
    const rest = Array.isArray(config.agentCommand) ? config.agentCommand.slice(1) : ["--dori-role", "lane", "--model", "{model}", "{prompt}"];
    config.stateDir = process.env.STATE_DIR;
    config.agentCommand = [process.env.SIDE_OMP, ...rest];
    changed = true;
  }
  // OMP loads omp-ulw as an extension; an empty path keeps the omp-dori extension from importing a second copy.
  if (config.ompUlwPath !== "") {
    config.ompUlwPath = "";
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
  omp-ulw:   $ulw_extension (OMP extension)
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
printf 'exec %q --no-extensions -e %q -e %q --config %q "$@"\n' "$omp_bin" "$src/extension" "$ulw" "$base/omp.yml" | wrapper omp
printf 'case "${1:-}" in\n  omp) shift; exec %q "$@" ;;\n  hc) shift; exec hc "$@" ;;\n  *) exec %q "$@" ;;\nesac\n' \
  "$base/bin/omp" "$base/bin/dori" | wrapper omp-dori
mkdir -p "$bin_dir"
link "$base/bin/omp-dori" "$bin_dir/omp-dori" || echo "Run $base/bin/omp-dori directly." >&2

cat <<EOF
Installed next to the Dori in $primary_home, which was not changed.
  Home:       $base (config.json, state, generated skill, omp.yml and wrappers)
  Command:    $bin_dir/omp-dori  (omp-dori <dori command> | omp-dori omp ... | omp-dori hc ...)
  Skill:      omp-dori-mode; dori-mode is hidden in omp-dori omp sessions
  Extensions: $src/extension and $ulw, loaded only by omp-dori omp, which discovers no other OMP extension
  omp-ulw:    $ulw
  hc session: $hc_session
  Config:     $config
Next: create a separate messenger bot and channel, set runtime, leadPane, defaultCwd, defaultModel and hooks
in $config, then start the lead in its own hc session:
  omp-dori hc run --name omp-dori-lead -- omp-dori omp --dori-role dori
Installation does not start a messenger, watcher or hc server.
EOF
