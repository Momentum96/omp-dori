#!/bin/sh
# Builds and signs the release assets of one Cataly version on this Linux x64 host, with the
# binaries of other targets built by CI:
#
#   sh scripts/release.sh <version> [--add <target>=<binary>]...
#
# It refuses a working tree with uncommitted changes and a <version> other than Cargo.toml's.
# The build is `cargo build --locked --release --features voice` into target/local-release, with the
# release public key, the commit and the commit time handed to cargo through `env` (never exported
# into this shell). Each --add names a binary of another target (linux-arm64, darwin-arm64,
# darwin-x64), as a ci dispatch with `binaries` uploads it (artifact cataly-<version>-<target>). It cannot
# run here, so it is checked without running it: `file` reports that target's executable format,
# and its bytes hold this release's version (the HTTP user agent `cataly/<version>`), this commit
# and the release key, so it was built from this commit with this key. Into release-local/v<version>,
# next to the repository, it writes:
#
#   cataly-<version>-<target>                 the binary of every target
#   cataly-<version>.ts                       the OMP extension (`cataly extension`), one for all
#   install.sh                                the installer, which must already hold the release key
#   LICENSE, THIRD-PARTY-NOTICES-<target>.txt the notices of every target
#   cataly-<version>.tgz, cataly-<target>-<version>.tgz   the npm and Bun packages (npm pack)
#   SHA256SUMS                                every file above
#   SHA256SUMS.sig                            hex ed25519 signature of SHA256SUMS
#
# The folder appears only when everything in it is written and the signature checks. Nothing is
# published, pushed or uploaded. The key pair lives in ~/.config/cataly-release: release.pub (64
# hex digits) and release.pem (the private key, used only by openssl).
#
# Needs cargo, file, git, grep, jq, npm, openssl, sha256sum and od.
set -eu

fail() {
	echo "release: $*" >&2
	exit 1
}

usage="usage: sh scripts/release.sh <version> [--add <target>=<binary>]..."
[ $# -ge 1 ] || fail "$usage"
version=$1
shift
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
keys="$HOME/.config/cataly-release"

# The extra binaries as space-separated "<target>=<absolute path>" words.
added=""
while [ $# -gt 0 ]; do
	[ "$1" = --add ] && [ $# -ge 2 ] || fail "$usage"
	target=${2%%=*}
	path=${2#*=}
	[ "$target" != "$2" ] && [ -n "$path" ] || fail "--add takes <target>=<binary>, not '$2'"
	case "$target" in
	linux-arm64 | darwin-arm64 | darwin-x64) ;;
	linux-x64) fail "linux-x64 is built here; --add takes linux-arm64, darwin-arm64 or darwin-x64" ;;
	*) fail "'$target' is not a release target (linux-arm64, darwin-arm64, darwin-x64)" ;;
	esac
	case " $added" in
	*" $target="*) fail "$target is added twice" ;;
	esac
	[ -f "$path" ] || fail "$path is not a file"
	path=$(cd "$(dirname "$path")" && pwd)/$(basename "$path")
	case "$path" in
	*[[:space:]]*) fail "'$path' holds white space; move the binary to a plain path" ;;
	esac
	added="$added $target=$path"
	shift 2
done

for tool in cargo file git grep jq npm openssl sha256sum od; do
	command -v "$tool" >/dev/null 2>&1 || fail "$tool is not installed"
done
[ "$(uname -s) $(uname -m)" = "Linux x86_64" ] || fail "release builds run on Linux x86_64"
[ -z "$(git status --porcelain)" ] || fail "the working tree has uncommitted changes; commit or remove them first"
manifest=$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ "$manifest" = "$version" ] || fail "Cargo.toml's version is $manifest, not $version"
for file in release.pub release.pem; do
	[ -f "$keys/$file" ] || fail "$keys/$file is missing"
done

key=$(tr -d ' \n' <"$keys/release.pub")
echo "$key" | grep -Eq '^[0-9a-f]{64}$' || fail "$keys/release.pub is not 64 lower-case hex digits"
# The private key must be the one the binary will trust.
pem_key=$(openssl pkey -in "$keys/release.pem" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')
[ "$pem_key" = "$key" ] || fail "release.pem does not belong to release.pub"
# install.sh is read straight from the repository's main branch, so it must hold this key already.
grep -q "^RELEASE_KEY=\"$key\"\$" install.sh || fail "install.sh does not hold the key in $keys/release.pub"
commit=$(git rev-parse HEAD)

# check_added <target> <binary>: the format of <target>, and this version, commit and key inside.
check_added() {
	case "$1" in
	linux-arm64) format="ELF 64-bit LSB.*ARM aarch64" ;;
	darwin-arm64) format="Mach-O 64-bit.*arm64" ;;
	darwin-x64) format="Mach-O 64-bit.*x86_64" ;;
	esac
	file -b "$2" | grep -Eq "$format" || fail "$2 is not a $1 executable: $(file -b "$2")"
	grep -aqF "cataly/$version" "$2" || fail "$2 is not version $version"
	grep -aqF "$commit" "$2" || fail "$2 was not built from commit $commit"
	grep -aqF "$key" "$2" || fail "$2 does not trust the release key in $keys/release.pub"
}

for pair in $added; do
	check_added "${pair%%=*}" "${pair#*=}"
done

out="$(dirname "$repo")/release-local/v$version"
[ ! -e "$out" ] || fail "$out exists; remove it to build $version again"
part="$out.part"
rm -rf "$part"
mkdir -p "$part"
work=$(mktemp -d "${TMPDIR:-/tmp}/cataly-release.XXXXXX") || fail "no temporary folder"
trap 'rm -rf "$work"' EXIT INT TERM

target_dir="$repo/target/local-release"
env CATALY_RELEASE_KEY="$key" CATALY_COMMIT="$commit" SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" \
	CARGO_TARGET_DIR="$target_dir" cargo build --locked --release --features voice ||
	fail "the build failed"
bin="$target_dir/release/cataly"
report=$("$bin" version --json) || fail "the built binary gave no version report"
[ "$(echo "$report" | jq -r .version)" = "$version" ] || fail "the built binary is not version $version"
[ "$(echo "$report" | jq -r .commit)" = "$commit" ] || fail "the built binary does not name commit $commit"
[ "$(echo "$report" | jq -r .target)" = linux-x64 ] || fail "the built binary is not for linux-x64"

binary="$part/cataly-$version-linux-x64"
cp "$bin" "$binary"
chmod 755 "$binary"
"$bin" extension >"$part/cataly-$version.ts"
cp install.sh "$part/"
cp LICENSE "$part/"
sh scripts/third-party-notices.sh x86_64-unknown-linux-gnu "$part/THIRD-PARTY-NOTICES-linux-x64.txt"
packs="linux-x64=$binary"
for pair in $added; do
	target=${pair%%=*}
	cp "${pair#*=}" "$part/cataly-$version-$target"
	chmod 755 "$part/cataly-$version-$target"
	case "$target" in
	linux-arm64) triple=aarch64-unknown-linux-gnu ;;
	darwin-arm64) triple=aarch64-apple-darwin ;;
	darwin-x64) triple=x86_64-apple-darwin ;;
	esac
	sh scripts/third-party-notices.sh "$triple" "$part/THIRD-PARTY-NOTICES-$target.txt"
	packs="$packs $target=$part/cataly-$version-$target"
done
chmod 644 "$part"/THIRD-PARTY-NOTICES-*.txt
# shellcheck disable=SC2086 # one word per target; release paths hold no white space
sh scripts/npm-pack.sh "$version" "$part" $packs

(
	cd "$part"
	for file in *; do
		case "$file" in
		SHA256SUMS*) ;;
		*) sha256sum -- "$file" ;;
		esac
	done >"$work/SHA256SUMS"
)
mv "$work/SHA256SUMS" "$part/SHA256SUMS"
openssl pkeyutl -sign -rawin -inkey "$keys/release.pem" -in "$part/SHA256SUMS" -out "$work/sig" ||
	fail "signing failed"
openssl pkeyutl -verify -rawin -inkey "$keys/release.pem" -sigfile "$work/sig" -in "$part/SHA256SUMS" >/dev/null ||
	fail "the signature does not verify"
od -An -v -tx1 "$work/sig" | tr -d ' \n' >"$part/SHA256SUMS.sig"

mv "$part" "$out"
echo "Cataly $version (commit $commit) in $out:"
ls -l "$out"
