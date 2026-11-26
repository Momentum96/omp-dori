#!/bin/sh
# Packs the npm and Bun packages of one Cataly version into <out> with `npm pack`:
#
#   sh scripts/npm-pack.sh <version> <out> <notices> <target>=<binary> [<target>=<binary> ...]
#
# For each target (linux-x64, darwin-arm64) it packs cataly-<target>: npm/cataly-<target>/package.json
# with <version>, the binary, the extension that binary prints, LICENSE and the <notices> file.
# Then it packs cataly: the launcher, the repository's README.md (its relative image paths and
# links made absolute, since npm resolves neither), LICENSE and <notices>, with an optional
# dependency on exactly the targets packed here, so a release never names a package it lacks.
# Every binary must report <version> and its target. The versions come from the arguments only;
# the package.json files in the repository keep 0.0.0-source.
#
# Needs a POSIX shell, jq and npm.
set -eu

fail() {
	echo "npm-pack: $*" >&2
	exit 1
}

[ $# -ge 4 ] || fail "usage: sh scripts/npm-pack.sh <version> <out> <notices> <target>=<binary> ..."
version=$1
out=$2
notices=$3
shift 3
repo=$(cd "$(dirname "$0")/.." && pwd)
for tool in jq npm; do
	command -v "$tool" >/dev/null 2>&1 || fail "$tool is not installed"
done
[ -d "$out" ] || fail "$out is not a folder"
[ -f "$notices" ] || fail "$notices is missing"

stage=$(mktemp -d "${TMPDIR:-/tmp}/cataly-npm.XXXXXX") || fail "no temporary folder"
trap 'rm -rf "$stage"' EXIT INT TERM

# stamp <package.json> <dest> [jq filter]: the file with <version> and the filter applied.
stamp() {
	jq --arg v "$version" --argjson targets "$targets_json" ".version = \$v | ${3:-.}" "$1" >"$2" ||
		fail "$1 could not be stamped"
}

targets=""
targets_json="[]"
for pair in "$@"; do
	target=${pair%%=*}
	binary=${pair#*=}
	case "$target" in
	linux-x64 | darwin-arm64) ;;
	*) fail "'$target' is not a release target (linux-x64, darwin-arm64)" ;;
	esac
	[ -x "$binary" ] || fail "$binary is not an executable file"
	report=$("$binary" version --json) || fail "$binary gave no version report"
	[ "$(echo "$report" | jq -r .version)" = "$version" ] || fail "$binary is not version $version"
	[ "$(echo "$report" | jq -r .target)" = "$target" ] || fail "$binary is not built for $target"

	dir="$stage/cataly-$target"
	mkdir "$dir"
	stamp "$repo/npm/cataly-$target/package.json" "$dir/package.json"
	cp "$binary" "$dir/cataly"
	chmod 755 "$dir/cataly"
	"$binary" extension >"$dir/cataly.ts"
	cp "$repo/LICENSE" "$dir/"
	cp "$notices" "$dir/THIRD-PARTY-NOTICES.txt"
	chmod 644 "$dir/THIRD-PARTY-NOTICES.txt"
	npm pack --silent --pack-destination "$out" "$dir" >/dev/null || fail "npm pack of cataly-$target failed"
	targets="$targets $target"
	targets_json=$(echo "$targets_json" | jq -c --arg t "$target" '. + [$t]')
done

dir="$stage/cataly"
mkdir -p "$dir/bin"
# shellcheck disable=SC2016 # a jq filter: $targets and $v are jq variables
stamp "$repo/npm/cataly/package.json" "$dir/package.json" \
	'.optionalDependencies = ($targets | map({key: ("cataly-" + .), value: $v}) | from_entries)'
cp "$repo/npm/cataly/bin/cataly.js" "$dir/bin/"
chmod 755 "$dir/bin/cataly.js"
raw=https://raw.githubusercontent.com/vntrevx/Cataly/main
blob=https://github.com/vntrevx/Cataly/blob/main
sed -E -e "s@(src|srcset)=\"([^\"#:]+)\"@\\1=\"$raw/\\2\"@g" \
	-e "s@href=\"([^\"#:]+)\"@href=\"$blob/\\1\"@g" \
	-e "s@\\]\\(([^)#:]+)\\)@]($blob/\\1)@g" "$repo/README.md" >"$dir/README.md" ||
	fail "README.md could not be copied"
cp "$repo/LICENSE" "$dir/"
cp "$notices" "$dir/THIRD-PARTY-NOTICES.txt"
chmod 644 "$dir/THIRD-PARTY-NOTICES.txt"
npm pack --silent --pack-destination "$out" "$dir" >/dev/null || fail "npm pack of cataly failed"
echo "packed cataly $version with$targets into $out"
