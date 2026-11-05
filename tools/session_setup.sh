#!/bin/sh
# Make a fresh machine or cloud container ready to build, test and gate this
# repository without a wait later: the packages the tests and the browser
# checks need, the Python gates' tools, the crates index, the CI test runner,
# and the fast-build configuration (tools/fast_rust_builds.py). Meant for a
# cloud environment's setup script; running it again changes nothing.
#
#     sh tools/session_setup.sh            everything
#     sh tools/session_setup.sh --no-apt   skip the apt packages (no root, or another distribution)
#     sh tools/session_setup.sh --tools-only   only the host's tool toolchain (the last bullet below)
#
# What each part is for:
# - apt: mold, clang and sccache (fast builds); bubblewrap and poppler-utils
#   (the document previews, docs/artifact-previews.md); openssh-client
#   (tests/test_compute_gce.py). LibreOffice is not installed: DOCX and PPTX
#   page previews need it, and it is large; install it when a task needs it.
# - pip: ruff and mypy, which python3 tools/check.py --fast runs, and the
#   docs site's markdown-it-py (tools/docs_site/requirements.lock).
# - cargo fetch --locked: the gates and CI build with --offline.
# - cargo-nextest: CI's test runner (tools/install_ci_tool.py pins its digest).
# - fast_rust_builds.py: mold, sccache and line tables only, in ~/.cargo/config.toml.
# - host tools (docs/tools-model.md, section 4): the shared tool store, pulled once with
#   `by tools pull` so turns link its skills and connector packages instead of copying
#   them, and Anvil's CLI and SDKs, installed once with `anvil install --prefix`. Each is
#   skipped with a message when it is not asked for or its program is absent:
#     BRANCHYARD_TOOLS_FROM=gs://BUCKET/PREFIX   pull the store from a bucket
#     BRANCHYARD_TOOLS_OCI=REGISTRY/REPOSITORY   or from Artifact Registry (needs oras)
#     BRANCHYARD_TOOLS_STORE=/opt/branchyard/store   where the store goes (the default)
#     BRANCHYARD_ANVIL_PREFIX=/opt/anvil         where Anvil's toolchain goes (the default)
set -eu
cd "$(dirname "$0")/.."

apt=yes
tools_only=no
for arg in "$@"; do
    case "$arg" in
        --no-apt) apt=no ;;
        --tools-only) tools_only=yes ;;
        *) echo "session_setup: unknown argument $arg" >&2; exit 2 ;;
    esac
done

sudo=""
[ "$(id -u)" = 0 ] || sudo=sudo

host_tools() {
    prefix="${BRANCHYARD_ANVIL_PREFIX:-/opt/anvil}"
    if command -v anvil >/dev/null 2>&1 && anvil install --help >/dev/null 2>&1; then
        $sudo anvil install --prefix "$prefix"
    else
        echo "session_setup: anvil with an install command is not on PATH; skipping its toolchain under $prefix" >&2
    fi
    store="${BRANCHYARD_TOOLS_STORE:-/opt/branchyard/store}"
    if [ -n "${BRANCHYARD_TOOLS_FROM:-}" ]; then
        source_flag=--from
        source_ref="$BRANCHYARD_TOOLS_FROM"
    elif [ -n "${BRANCHYARD_TOOLS_OCI:-}" ]; then
        source_flag=--oci
        source_ref="$BRANCHYARD_TOOLS_OCI"
    else
        echo "session_setup: BRANCHYARD_TOOLS_FROM and BRANCHYARD_TOOLS_OCI are not set; skipping the tool store" >&2
        return 0
    fi
    if ! command -v by >/dev/null 2>&1; then
        echo "session_setup: by is not on PATH; skipping the tool store" >&2
        return 0
    fi
    $sudo install -d -o "$(id -u)" -g "$(id -g)" -m 0755 "$store"
    by tools pull "$source_flag" "$source_ref" --store "$store"
}

if [ "$tools_only" = yes ]; then
    host_tools
    echo "session_setup: done"
    exit 0
fi

if [ "$apt" = yes ] && command -v apt-get >/dev/null 2>&1; then
    $sudo apt-get update -qq
    $sudo apt-get install -y -qq --no-install-recommends \
        mold clang sccache bubblewrap poppler-utils openssh-client pkg-config libcap-ng-dev
fi

python3 -m pip install -q --disable-pip-version-check ruff mypy
python3 -m pip install -q --disable-pip-version-check --require-hashes -r tools/docs_site/requirements.lock

cargo fetch --locked

if ! command -v cargo-nextest >/dev/null 2>&1; then
    python3 tools/install_ci_tool.py cargo-nextest
fi

python3 tools/fast_rust_builds.py
host_tools
echo "session_setup: done"
