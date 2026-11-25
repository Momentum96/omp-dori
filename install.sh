#!/bin/sh
# Cataly installer (IR-01, DESIGN 11.4). It only downloads and checks hashes; the verified binary's
# own `cataly install` re-checks the hashes, verifies the signature of SHA256SUMS with the release
# key compiled into it, checks the executable format and the binary's version report, and only then
# switches. Nothing is started, no configuration is written, no service is enabled.
#
#   sh install.sh             the latest stable release
#   sh install.sh v1.2.3      that release (prereleases included)
#   sh install.sh <folder>    release assets already in a local folder
#
# Needs a POSIX shell, curl, uname, and sha256sum or shasum.
set -eu

REPO="vntrevx/cataly-core"
# The ed25519 public key releases are signed with (hex); the publish job fills it in.
RELEASE_KEY="@RELEASE_KEY@"

fail() {
	echo "cataly install: $*" >&2
	exit 1
}

case "$(uname -s) $(uname -m)" in
"Linux x86_64") target=linux-x64 ;;
"Darwin arm64") target=darwin-arm64 ;;
*) fail "this host is $(uname -s) $(uname -m); Cataly runs on Linux x86_64 and macOS arm64" ;;
esac

if command -v sha256sum >/dev/null 2>&1; then
	hash_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
	hash_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
	fail "neither sha256sum nor shasum is installed"
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/cataly-install.XXXXXX") || fail "no temporary folder"
chmod 700 "$tmp"
trap 'rm -rf "$tmp"' EXIT INT TERM

# fetch <url> <file> <max bytes>: HTTPS only, redirects to HTTPS only, bounded.
fetch() {
	command -v curl >/dev/null 2>&1 || fail "curl is not installed"
	curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 10 --max-time 120 \
		--max-filesize "$3" -o "$tmp/$2" "$1" || fail "download of $2 failed or exceeded its limit"
	size=$(wc -c <"$tmp/$2" | tr -d ' ')
	[ "$size" -le "$3" ] || fail "$2 is larger than $3 bytes"
}

source_dir=""
case "${1:-}" in
"") tag="" ;;
v[0-9]*) tag="$1" ;;
*)
	[ -d "$1" ] || fail "'$1' is neither a release tag (v1.2.3) nor a folder"
	source_dir=$(cd "$1" && pwd)
	tag=""
	;;
esac

if [ -n "$source_dir" ]; then
	sums="$source_dir/SHA256SUMS"
	[ -f "$sums" ] || fail "$source_dir has no SHA256SUMS"
	cp "$sums" "$tmp/SHA256SUMS"
	[ -f "$source_dir/SHA256SUMS.sig" ] && cp "$source_dir/SHA256SUMS.sig" "$tmp/SHA256SUMS.sig"
	version=$(sed -n "s/^[0-9a-f]\{64\}  cataly-\(.*\)-$target\$/\1/p" "$tmp/SHA256SUMS")
	[ -n "$version" ] || fail "SHA256SUMS lists no binary for $target"
	cp "$source_dir/cataly-$version-$target" "$tmp/" 2>/dev/null || fail "cataly-$version-$target is missing"
	cp "$source_dir/cataly-$version.ts" "$tmp/" 2>/dev/null || fail "cataly-$version.ts is missing"
else
	if [ -z "$tag" ]; then
		command -v curl >/dev/null 2>&1 || fail "curl is not installed"
		curl --proto '=https' --proto-redir '=https' -fsSL --connect-timeout 10 --max-time 15 \
			--max-filesize 1048576 -o "$tmp/latest.json" "https://api.github.com/repos/$REPO/releases/latest" ||
			fail "no stable release is published yet (or GitHub is unreachable); pass an exact tag such as v0.1.0"
		tag=$(sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\(v[^"]*\)".*/\1/p' "$tmp/latest.json" | head -n 1)
		[ -n "$tag" ] || fail "no stable release is published yet; pass an exact tag such as v0.1.0"
	fi
	version=${tag#v}
	base="https://github.com/$REPO/releases/download/$tag"
	fetch "$base/SHA256SUMS" SHA256SUMS 65536
	fetch "$base/SHA256SUMS.sig" SHA256SUMS.sig 1024
	fetch "$base/cataly-$version-$target" "cataly-$version-$target" 134217728
	fetch "$base/cataly-$version.ts" "cataly-$version.ts" 1048576
fi

for asset in "cataly-$version-$target" "cataly-$version.ts"; do
	lines=$(grep -c "  $asset\$" "$tmp/SHA256SUMS" || true)
	[ "$lines" = 1 ] || fail "SHA256SUMS must list $asset exactly once"
	want=$(grep "  $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
	[ "$(hash_of "$tmp/$asset")" = "$want" ] || fail "$asset does not match SHA256SUMS; nothing was installed"
done

echo "Cataly $version for $target; release key $RELEASE_KEY"
chmod 700 "$tmp/cataly-$version-$target"
"$tmp/cataly-$version-$target" install --from "$tmp"
