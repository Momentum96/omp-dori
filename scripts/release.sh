#!/bin/sh
# Builds and signs the release assets of one Cataly version on this Linux x64 host:
#
#   sh scripts/release.sh <version>
#
# It refuses a working tree with uncommitted changes and a <version> other than Cargo.toml's.
# The build is `cargo build --locked --release --features voice` into target/local-release, with the
# release public key, the commit and the commit time handed to cargo through `env` (never exported
# into this shell). Into release-local/v<version>, next to the repository, it writes:
#
#   cataly-<version>-linux-x64        the binary
#   cataly-<version>.ts               its OMP extension (`cataly extension`)
#   install.sh                        the installer with the release key filled in
#   LICENSE, THIRD-PARTY-NOTICES.txt
#   cataly-<version>.tgz, cataly-linux-x64-<version>.tgz   the npm and Bun packages (npm pack)
#   SHA256SUMS                        every file above
#   SHA256SUMS.sig                    hex ed25519 signature of SHA256SUMS
#
# The folder appears only when everything in it is written and the signature checks. Nothing is
# published, pushed or uploaded. The key pair lives in ~/.config/cataly-release: release.pub (64
# hex digits) and release.pem (the private key, used only by openssl).
#
# A darwin-arm64 binary comes from CI; its release adds `darwin-arm64=<binary>` to the
# scripts/npm-pack.sh call and its binary to the folder before SHA256SUMS is written.
#
# Needs cargo, git, jq, npm, openssl, sha256sum and od.
set -eu

fail() {
	echo "release: $*" >&2
	exit 1
}

[ $# = 1 ] || fail "usage: sh scripts/release.sh <version>"
version=$1
repo=$(cd "$(dirname "$0")/.." && pwd)
cd "$repo"
keys="$HOME/.config/cataly-release"

for tool in cargo git jq npm openssl sha256sum od; do
	command -v "$tool" >/dev/null 2>&1 || fail "$tool is not installed"
done
[ "$(uname -s) $(uname -m)" = "Linux x86_64" ] || fail "release builds run on Linux x86_64"
[ -z "$(git status --porcelain)" ] || fail "the working tree has uncommitted changes; commit or remove them first"
manifest=$(sed -n '/^\[package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ "$manifest" = "$version" ] || fail "Cargo.toml's version is $manifest, not $version"
for file in release.pub release.pem; do
	[ -f "$keys/$file" ] || fail "$keys/$file is missing"
done

# public/ is what the release repository shows; its copies must be the ones released.
for file in LICENSE install.sh assets/banner.jpg; do
	cmp -s "$file" "public/$file" || fail "public/$file differs from $file; copy $file there first"
done
key=$(tr -d ' \n' <"$keys/release.pub")
echo "$key" | grep -Eq '^[0-9a-f]{64}$' || fail "$keys/release.pub is not 64 lower-case hex digits"
# The private key must be the one the binary will trust.
pem_key=$(openssl pkey -in "$keys/release.pem" -pubout -outform DER | tail -c 32 | od -An -v -tx1 | tr -d ' \n')
[ "$pem_key" = "$key" ] || fail "release.pem does not belong to release.pub"

out="$(dirname "$repo")/release-local/v$version"
[ ! -e "$out" ] || fail "$out exists; remove it to build $version again"
part="$out.part"
rm -rf "$part"
mkdir -p "$part"
work=$(mktemp -d "${TMPDIR:-/tmp}/cataly-release.XXXXXX") || fail "no temporary folder"
trap 'rm -rf "$work"' EXIT INT TERM

commit=$(git rev-parse HEAD)
target_dir="$repo/target/local-release"
env CATALY_RELEASE_KEY="$key" CATALY_COMMIT="$commit" SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)" \
	CARGO_TARGET_DIR="$target_dir" cargo build --locked --release --features voice ||
	fail "the build failed"
bin="$target_dir/release/cataly"
report=$("$bin" version --json) || fail "the built binary gave no version report"
[ "$(echo "$report" | jq -r .version)" = "$version" ] || fail "the built binary is not version $version"
[ "$(echo "$report" | jq -r .commit)" = "$commit" ] || fail "the built binary does not name commit $commit"

binary="$part/cataly-$version-linux-x64"
cp "$bin" "$binary"
chmod 755 "$binary"
"$bin" extension >"$part/cataly-$version.ts"
sed "s/@RELEASE_KEY@/$key/" install.sh >"$part/install.sh"
grep -q "^RELEASE_KEY=\"$key\"\$" "$part/install.sh" || fail "install.sh has no release key line"
cp LICENSE "$part/"
sh scripts/third-party-notices.sh x86_64-unknown-linux-gnu "$part/THIRD-PARTY-NOTICES.txt"
chmod 644 "$part/THIRD-PARTY-NOTICES.txt"
sh scripts/npm-pack.sh "$version" "$part" "$part/THIRD-PARTY-NOTICES.txt" "linux-x64=$binary"

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
