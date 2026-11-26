#!/bin/sh
# Cataly installer (IR-01, DESIGN 11.4). It only downloads and checks hashes; the verified binary's
# own `cataly install` re-checks the hashes, verifies the signature of SHA256SUMS with the release
# key compiled into it, checks the executable format and the binary's version report, and only then
# switches. Nothing is started, no configuration is written, no service is enabled.
#
#   sh install.sh             the newest stable Cataly release
#   sh install.sh v1.2.3      that release (prereleases included)
#   sh install.sh <folder>    release assets already in a local folder
#
# Needs a POSIX shell, curl, uname, and sha256sum or shasum.
set -eu

REPO="vntrevx/Cataly"
# The ed25519 public key releases are signed with (hex); scripts/release.sh refuses to sign with
# any other key.
RELEASE_KEY="b35b95383bf865e896c2bdcc0b5b4580c4c55a23b5cd401fdafc63ead72a3b9d"

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
			--max-filesize 1048576 -o "$tmp/releases.json" "https://api.github.com/repos/$REPO/releases?per_page=30" ||
			fail "GitHub's release list is unreachable; try again, or pass an exact tag such as v0.2.0"
		# The repository also holds prereleases and releases of other software. A Cataly release is
		# tagged v<major>.<minor>.<patch> and carries SHA256SUMS and cataly-<version>.ts; the newest
		# stable one that is neither a draft nor a prerelease is chosen. The list is read as JSON:
		# records split at quotes alternate between structure and string text (an escaped quote
		# continues its string), so field order and text inside strings do not matter. Each line
		# printed: major minor patch tag, then 1 when it also carries this host's binary and the
		# signature.
		newest=$(awk -v target="$target" '
			function reset() { tag = ""; draft = 0; pre = 0; assets = "\n" }
			function has(name) { return index(assets, "\n" name "\n") > 0 }
			function flush(  v, n) {
				if (tag !~ /^v[0-9]+\.[0-9]+\.[0-9]+$/ || draft || pre) return
				v = substr(tag, 2)
				if (!has("SHA256SUMS") || !has("cataly-" v ".ts")) return
				split(v, n, ".")
				print n[1], n[2], n[3], tag, (has("cataly-" v "-" target) && has("SHA256SUMS.sig")) ? 1 : 0
			}
			function value(v) {
				expecting = 0
				if (depth == 1 && key == "tag_name") tag = v
				if (depth == 1 && key == "draft") draft = (v == "true")
				if (depth == 1 && key == "prerelease") pre = (v == "true")
				if (depth == 2 && key == "name") assets = assets v "\n"
			}
			function word_end() {
				if (word != "") value(word)
				word = ""
			}
			function structure(t,  i, c, n) {
				n = length(t)
				for (i = 1; i <= n; i++) {
					c = substr(t, i, 1)
					if (c ~ /[A-Za-z0-9.+-]/) {
						if (expecting) word = word c
						continue
					}
					word_end()
					if (c == ":") { key = last; expecting = 1 }
					else if (c == "[") expecting = 0
					else if (c == "{") { expecting = 0; depth++; if (depth == 1) reset() }
					else if (c == "}") { if (depth == 1) flush(); depth-- }
				}
				word_end()
			}
			BEGIN { RS = "\"" }
			{
				if (!inside) { structure($0); inside = 1; text = ""; next }
				text = text $0
				n = 0
				while (n < length($0) && substr($0, length($0) - n, 1) == "\\") n++
				if (n % 2 == 1) { text = text "\""; next }
				inside = 0
				if (expecting) value(text)
				else last = text
			}
		' "$tmp/releases.json" | sort -k1,1n -k2,2n -k3,3n | tail -n 1)
		[ -n "$newest" ] || fail "no stable Cataly release is published yet; pass an exact tag such as v0.2.0"
		tag=$(echo "$newest" | cut -d' ' -f4)
		[ "$(echo "$newest" | cut -d' ' -f5)" = 1 ] ||
			fail "release $tag has no cataly-${tag#v}-$target or no SHA256SUMS.sig; it cannot be installed on this host"
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
