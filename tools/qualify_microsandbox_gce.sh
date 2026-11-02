#!/bin/sh
# Live, model-free runtime qualification on an already prepared Linux/KVM worker.
# Usage: sh qualify_microsandbox_gce.sh ARCHIVE SHA256 EVIDENCE_DIRECTORY
# Docker must already have alpine:3.20, or BY_MSB_DOCKER_IMAGE, locally.
set -eu
archive=${1:?runtime archive required}
checksum=${2:?archive SHA256 required}
evidence=${3:?new evidence directory required}
image=${BY_MSB_DOCKER_IMAGE:-alpine:3.20}
case "$evidence" in /*) ;; *) echo 'evidence directory must be absolute' >&2; exit 2 ;; esac
test ! -e "$evidence"
mkdir -m 700 -p "$evidence"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum -c -
test -r /dev/kvm
test -w /dev/kvm
export MSB_HOME
MSB_HOME=$(mktemp -d /tmp/by-msb-qual.XXXXXX)
msb="$MSB_HOME/bin/msb"
name=
trap 'if [ -n "$name" ]; then "$msb" sandbox remove --force "$name" >/dev/null 2>&1 || true; fi; rm -rf "$MSB_HOME"' EXIT HUP INT TERM
mkdir "$MSB_HOME/bin" "$MSB_HOME/lib"
tar -xzf "$archive" -C "$MSB_HOME/bin" --no-same-owner
mv "$MSB_HOME/bin/libkrunfw.so.5.6.1" "$MSB_HOME/lib/"
cp "$MSB_HOME/bin/runtime-provenance.json" "$evidence/"
"$msb" --version | tee "$evidence/runtime-version.txt"
uname -r > "$evidence/host-kernel.txt"
docker image inspect "$image" --format '{{json .RepoDigests}}' > "$evidence/image-digests.json"
docker save "$image" | "$msb" image load --tag branchyard-qual-alpine
printf 'host-only qualification marker\n' > "$evidence/host-sentinel"
for memory in 128 512 1024; do
    name="by-qual-$memory"
    "$msb" sandbox create branchyard-qual-alpine --name "$name" --cpus 1 --memory "${memory}M"
    "$msb" sandbox exec "$name" --no-tty --timeout 30s -- uname -r < /dev/null > "$evidence/guest-kernel-$memory.txt"
    test "$(cat "$evidence/guest-kernel-$memory.txt")" != "$(cat "$evidence/host-kernel.txt")"
    "$msb" sandbox exec "$name" --no-tty --timeout 30s -- nproc < /dev/null > "$evidence/cpus-$memory.txt"
    test "$(cat "$evidence/cpus-$memory.txt")" = 1
    "$msb" sandbox exec "$name" --no-tty --timeout 30s -- cat /proc/meminfo < /dev/null > "$evidence/meminfo-$memory.txt"
    kib=$(awk '/^MemTotal:/ {print $2}' "$evidence/meminfo-$memory.txt")
    printf 'requested_mib=%s guest_memtotal_kib=%s limit_kib=%s\n' "$memory" "$kib" "$((memory * 1024))"
    test "$kib" -le "$((memory * 1024))"
    "$msb" sandbox exec "$name" --no-tty --timeout 30s -- sh -c 'printf roundtrip > /tmp/roundtrip; cat /tmp/roundtrip' < /dev/null > "$evidence/roundtrip-$memory.txt"
    test "$(cat "$evidence/roundtrip-$memory.txt")" = roundtrip
    "$msb" sandbox exec "$name" --no-tty --timeout 30s -- sh -c 'test ! -e "$1"' probe "$evidence/host-sentinel" < /dev/null
    "$msb" sandbox inspect "$name" > "$evidence/inspect-$memory.txt"
    "$msb" sandbox remove --force "$name"
    name=
done
"$msb" sandbox list --format json > "$evidence/sandboxes-after.json"
test "$(tr -d '[:space:]' < "$evidence/sandboxes-after.json")" = '[]'
