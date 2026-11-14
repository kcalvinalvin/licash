#!/bin/sh
# Materialize the pinned official neutrino release with Licash's patches.
#
#   materialize.sh          write upstream/neutrino
#   materialize.sh --check  fail unless it is exactly what this would write
#
# The release is fetched by the Go toolchain and its content hash is
# recomputed here and compared with SUM, so the tree is the official one
# whatever the module cache holds. Everything that differs from upstream is
# in patches/, applied in name order with no fuzz.
set -eu

MODULE=github.com/lightninglabs/neutrino
VERSION=v0.18.0
# The module's h1 hash, as the Go checksum database records it.
SUM='h1:UsyeU3twkCSAXxUssVg8vGXcHkWKzHZ3xQCej57t5t0='

here=$(cd "$(dirname "$0")" && pwd)
out="$here/neutrino"
mode=${1:-write}
case "$mode" in
write | --check) ;;
*)
	echo "usage: $0 [--check]" >&2
	exit 2
	;;
esac

work=$(mktemp -d)
trap 'chmod -R u+w "$work" 2>/dev/null || true; rm -rf "$work"' EXIT

# A throwaway module: the download must not read or change chain/go.mod.
mkdir "$work/fetch"
printf 'module fetch\n\ngo 1.24\n' >"$work/fetch/go.mod"
(cd "$work/fetch" && GOFLAGS=-mod=mod go mod download "$MODULE@$VERSION")
src="$(go env GOMODCACHE)/$MODULE@$VERSION"
[ -d "$src" ] || {
	echo "materialize: $src is missing after the download" >&2
	exit 1
}

# Go's h1: SHA-256 over one "<sha256>  <module>@<version>/<path>" line per
# file, in byte order of the paths.
hex() { od -An -tx1 | tr -d ' \n'; }
got=$(cd "$src" && find . -type f | sed 's|^\./||' | LC_ALL=C sort |
	while IFS= read -r f; do
		printf '%s  %s\n' "$(sha256sum <"$f" | cut -d' ' -f1)" "$MODULE@$VERSION/$f"
	done | sha256sum | cut -d' ' -f1)
want=$(printf '%s' "${SUM#h1:}" | base64 -d | hex)
[ "$got" = "$want" ] || {
	echo "materialize: $MODULE@$VERSION does not match the pinned hash" >&2
	echo "  pinned   $want" >&2
	echo "  found    $got" >&2
	exit 1
}

tree="$work/neutrino"
cp -R "$src" "$tree"
chmod -R u+w "$tree"
for p in "$here"/patches/*.patch; do
	[ -e "$p" ] || continue
	(cd "$tree" && patch -p1 -F0 -s --no-backup-if-mismatch <"$p") || {
		echo "materialize: $(basename "$p") does not apply to $VERSION" >&2
		exit 1
	}
done

if [ "$mode" = --check ]; then
	diff -r "$tree" "$out" >/dev/null 2>&1 || {
		echo "materialize: $out is stale or edited; run $0" >&2
		exit 1
	}
	echo "neutrino $VERSION with patches: up to date"
	exit 0
fi

rm -rf "$out"
mv "$tree" "$out"
echo "neutrino $VERSION with patches: $out"
