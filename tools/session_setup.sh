#!/bin/sh
# Make a fresh machine or cloud container ready to build, test and gate this
# repository without a wait later: the packages the tests and the browser
# checks need, the Python gates' tools, the crates index, the CI test runner,
# and the fast-build configuration (tools/fast_rust_builds.py). Meant for a
# cloud environment's setup script; running it again changes nothing.
#
#     sh tools/session_setup.sh            everything
#     sh tools/session_setup.sh --no-apt   skip the apt packages (no root, or another distribution)
#
# What each part is for:
# - apt: mold, clang and sccache (fast builds); bubblewrap and poppler-utils
#   (the document previews, docs/artifact-previews.md); openssh-client
#   (tests/test_compute_gce.py). LibreOffice is not installed: DOCX and PPTX
#   page previews need it, and it is large; install it when a task needs it.
# - pip: ruff and mypy, which python3 tools/check.py --fast runs.
# - cargo fetch --locked: the gates and CI build with --offline.
# - cargo-nextest: CI's test runner (tools/install_ci_tool.py pins its digest).
# - fast_rust_builds.py: mold, sccache and line tables only, in ~/.cargo/config.toml.
set -eu
cd "$(dirname "$0")/.."

apt=yes
for arg in "$@"; do
    case "$arg" in
        --no-apt) apt=no ;;
        *) echo "session_setup: unknown argument $arg" >&2; exit 2 ;;
    esac
done

if [ "$apt" = yes ] && command -v apt-get >/dev/null 2>&1; then
    sudo=""
    [ "$(id -u)" = 0 ] || sudo=sudo
    $sudo apt-get update -qq
    $sudo apt-get install -y -qq --no-install-recommends \
        mold clang sccache bubblewrap poppler-utils openssh-client
fi

python3 -m pip install -q --disable-pip-version-check ruff mypy

cargo fetch --locked

if ! command -v cargo-nextest >/dev/null 2>&1; then
    python3 tools/install_ci_tool.py cargo-nextest
fi

python3 tools/fast_rust_builds.py
echo "session_setup: done"
