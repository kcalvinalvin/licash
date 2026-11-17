#!/bin/sh
# Lichen (github.com/lichenbtc-org/lichen), which the contract builds on,
# at the revision it is tested with: copied from its git checkout beside
# this repository (or LICHEN_GIT) into vendor/lichen. Its workspace takes
# lichen-btc, lichen-wots and lichen-risc0 from git. Temporary, until
# Lichen is published.
set -eu
cd "$(dirname "$0")"
LICHEN_REV=5cd7efc768270ccf42eb9f5a8ee3f1158b12555c

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
