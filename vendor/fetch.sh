#!/bin/sh
# Lichen and ln-gap, which the contract builds on, at the revisions it is
# tested with: copied from their git checkouts beside this repository (or
# LICHEN_GIT, LNGAP_GIT) into vendor/lichen and vendor/ln-gap, as they lie
# beside each other there. Temporary, until both are published.
set -eu
cd "$(dirname "$0")"
LICHEN_REV=de1c3f17bb2b6938626cee8f024a9c1d6e5aa846
LNGAP_REV=d7972035c12247a07b1dcc1b32e1682cd790d276

copy() { # name, git dir, revision
    if [ "$(cat "$1/.rev" 2>/dev/null)" = "$3" ]; then return; fi
    git -C "$2" cat-file -e "$3^{commit}" || {
        echo "$2 has no commit $3: fetch it there first" >&2
        exit 1
    }
    rm -rf "$1.new" && mkdir "$1.new"
    git -C "$2" archive "$3" | tar -x -C "$1.new"
    echo "$3" > "$1.new/.rev"
    rm -rf "$1" && mv "$1.new" "$1"
}

copy lichen "${LICHEN_GIT:-../../lichen}" "$LICHEN_REV"
copy ln-gap "${LNGAP_GIT:-../../ln-gap}" "$LNGAP_REV"
