#!/bin/sh
set -eu
umask 077

if [ "$#" -ne 3 ]; then
    printf 'Usage: %s SIGNED_PROBE GUEST_ASSETS WORK_PARENT\n' "$0" >&2
    exit 2
fi

probe=$1
assets=$2
fixtures=$(CDPATH='' cd "$(dirname "$0")" && pwd)
codesign --verify --strict "$probe"
scratch=$(mktemp -d "$3/nautilus-parity.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT

check_case() {
    name=$1
    expected_status=$2
    expected_output=$3
    shift 3
    work="$scratch/$name"
    output="$scratch/$name.out"
    mkdir "$work"

    status=0
    "$probe" pod "$assets" "$work" "$fixtures/$name.json" \
        --input "$fixtures/input.txt" --output "$output" "$@" \
        >"$scratch/$name.stdout" 2>"$scratch/$name.stderr" || status=$?
    # The owner intentionally keeps an empty directory for future session locks.
    remaining=$(find "$work" -mindepth 1 ! -path "$work/.nautilus-sessions" -print -quit)
    if [ "$status" -ne "$expected_status" ] ||
        ! cmp -s "$expected_output" "$output" ||
        [ "$(stat -f %Lp "$output")" != 600 ] ||
        [ -s "$scratch/$name.stdout" ] ||
        [ -n "$remaining" ]; then
        printf 'FAIL %s: observed exit %s, expected %s\n' "$name" "$status" "$expected_status" >&2
        [ -z "$remaining" ] || printf 'Remaining scratch: %s\n' "$remaining" >&2
        cmp "$expected_output" "$output" >&2 || :
        cat "$scratch/$name.stderr" >&2
        return 1
    fi
    if [ "$name" = success ]; then
        sed -n '1s/^/Mac sandbox status: /p' "$scratch/$name.stderr"
        grep '^PARITY_' "$scratch/$name.stderr"
    fi
    printf 'PASS %s: exit %s, matching mode-0600 export, session state reclaimed\n' "$name" "$status"
}

check_case success 23 "$fixtures/input.txt"
printf 'timeout-ready\n' >"$scratch/timeout.expected"
check_case timeout 124 "$scratch/timeout.expected" --timeout 8
