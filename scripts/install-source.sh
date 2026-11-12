#!/usr/bin/env bash
# Installs the complete OMP Dori candidate: skill CLI, harness sources, extension, and pinned omp-ulw.
# DORI_NO_PULL=1 (or true, yes) installs an existing checkout without updating it.
# Optional paths: DORI_SRC, OMP_ULW_SRC, SKILLS_DIR, OMP_EXTENSIONS_DIR.
set -euo pipefail

fail() { echo "$1" >&2; exit 1; }
absolute() { case "$1" in /*) printf '%s\n' "$1" ;; *) printf '%s/%s\n' "$PWD" "$1" ;; esac; }
at_least() { [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | { read -r first; printf '%s\n' "$first"; })" = "$1" ]; }

repo="https://github.com/vntrevx/omp-dori.git"
ulw_repo="https://github.com/dbc-hbin/omp-ulw.git"
src="$(absolute "${DORI_SRC:-$HOME/.dori/src}")"
ulw="$(absolute "${OMP_ULW_SRC:-$HOME/.dori/omp-ulw}")"
skills="$(absolute "${SKILLS_DIR:-$HOME/.agents/skills}")"
extensions="$(absolute "${OMP_EXTENSIONS_DIR:-$HOME/.omp/agent/extensions}")"
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
    echo "To keep it and install the checkout as it is, run scripts/install-source.sh again with DORI_NO_PULL=1." >&2
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

mkdir -p "$skills" "$extensions"
link "$src/skills/dori-mode" "$skills/dori-mode" || true
link "$src/extension" "$extensions/omp-dori" || fail "extension link was not installed"

# The skill owns the installed dori name. The complete harness is also reachable through dori harness.
(cd "$src" && bun install --frozen-lockfile --silent)
(cd "$src/skills/dori-mode/scripts" && bun install --frozen-lockfile --silent && bun link --silent)
(cd "$src/extension" && bun install --frozen-lockfile --silent)

mkdir -p "$HOME/.dori"
config="$HOME/.dori/config.json"
[ -f "$config" ] || cp "$src/skills/dori-mode/references/config.example.json" "$config"
CONFIG="$config" ULW_INDEX="$ulw/index.ts" ULW_EXPLICIT="${OMP_ULW_SRC:+1}" bun -e '
  const path = process.env.CONFIG, index = process.env.ULW_INDEX;
  const config = await Bun.file(path).json();
  if (process.env.ULW_EXPLICIT || !config.ompUlwPath || config.ompUlwPath === "~/.dori/omp-ulw/index.ts") {
    config.ompUlwPath = index;
    await Bun.write(path, JSON.stringify(config, null, 2) + "\n");
  }'

command -v herdr >/dev/null || command -v hc >/dev/null || echo "Install herdr or hc before launching sessions." >&2
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
