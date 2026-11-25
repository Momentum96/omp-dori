#!/bin/sh
# Writes THIRD-PARTY-NOTICES.txt for the release build (`--features voice`): every third-party
# crate linked into the binary for one target (normal dependencies only; build scripts, proc
# macros and dev-dependencies are not shipped), with its version, its licence expression and the
# licence text files the crate ships (LICENSE*, LICENCE*, COPYING*, NOTICE*, UNLICENSE*).
#
#   sh scripts/third-party-notices.sh [target-triple] [output-file]
#
# Defaults: the host's triple and THIRD-PARTY-NOTICES.txt in the current folder.
# Needs cargo and jq. Reads only the locked dependency graph and the crates cargo has downloaded.
set -eu

target="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
out="${2:-THIRD-PARTY-NOTICES.txt}"
root="$(cd "$(dirname "$0")/.." && pwd)"

cargo fetch --locked --manifest-path "$root/Cargo.toml" --target "$target" >/dev/null
meta="$(cargo metadata --locked --offline --format-version 1 --features voice \
	--filter-platform "$target" --manifest-path "$root/Cargo.toml")"

# The crates reached from the workspace members through normal edges, minus the members. A proc
# macro runs only in the compiler, so neither it nor what it depends on is reached through it.
crates="$(printf '%s' "$meta" | jq -r '
	.workspace_members as $ws
	| ([.packages[] | select(any(.targets[]; .kind | index("proc-macro"))) | {key: .id, value: true}]
	   | from_entries) as $macro
	| (.resolve.nodes | map({key: .id, value: .}) | from_entries) as $nodes
	| def walk_ids($todo; $seen):
		if ($todo | length) == 0 then $seen
		else
			($todo[0]) as $id
			| if $seen[$id] or $macro[$id] then walk_ids($todo[1:]; $seen)
			  else
				[$nodes[$id].deps[]
				 | select(any(.dep_kinds[]; .kind == null))
				 | .pkg] as $next
				| walk_ids($todo[1:] + $next; $seen + {($id): true})
			  end
		end;
	walk_ids($ws; {}) as $reached
	| .packages[]
	| select($reached[.id] and ((.id as $i | $ws | index($i)) | not))
	| [.name, .version, (.license // "see licence files"), (.manifest_path | sub("/Cargo.toml$"; ""))]
	| @tsv' | sort -t "$(printf '\t')" -k1,1 -k2,2V)"

tmp="$(mktemp "${out}.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
{
	echo "Third-party software in Cataly (target $target, features: voice)"
	echo
	echo "Cataly links the following crates. Each is listed with its version and licence,"
	echo "followed by the licence and notice files it ships."
	echo
	printf '%s\n' "$crates" | while IFS="$(printf '\t')" read -r name version license dir; do
		printf '%s %s: %s\n' "$name" "$version" "$license"
	done
	printf '%s\n' "$crates" | while IFS="$(printf '\t')" read -r name version license dir; do
		echo
		echo "================================================================================"
		printf '%s %s\nLicence: %s\n' "$name" "$version" "$license"
		found=0
		for f in "$dir"/LICENSE* "$dir"/LICENCE* "$dir"/license* "$dir"/COPYING* "$dir"/NOTICE* "$dir"/UNLICENSE*; do
			[ -f "$f" ] || continue
			found=1
			echo "--------------------------------------------------------------------------------"
			echo "${f##*/}:"
			echo
			cat "$f"
		done
		if [ "$found" = 0 ]; then
			echo "(the crate ships no licence file; its licence is the expression above)"
		fi
	done
} >"$tmp"
mv "$tmp" "$out"
trap - EXIT
echo "wrote $out ($(printf '%s\n' "$crates" | wc -l | tr -d ' ') crates)"
