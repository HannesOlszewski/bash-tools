#!/usr/bin/env bash
# Build and run the whole test suite inside a Linux container.
# usage: docker/run-tests.sh [arch|ubuntu]
set -euo pipefail
cd "$(dirname "$0")/.."
img=${1:-arch}
platform=()
# archlinux only publishes amd64 images
[[ $img == arch ]] && platform=(--platform linux/amd64)
docker build -q "${platform[@]}" -t bash-tools-test-$img -f docker/Dockerfile.$img docker >/dev/null
docker run --rm -t "${platform[@]}" -v "$PWD":/src:ro -e CARGO_TARGET_DIR=/tmp/target bash-tools-test-$img bash -c '
  set -e
  cp -r /src /work/repo && cd /work/repo
  bash --version | head -1
  cargo test --release -q 2>&1 | tail -40
  cargo build --release -q
  BASH_TOOLS_BIN=/tmp/target/release/bash-tools BASH_TOOLS_TEST_BASH=/bin/bash python3 tests/pty/test_e2e.py
  BASH_TOOLS_BIN=/tmp/target/release/bash-tools BASH_TOOLS_TEST_BASH=/bin/bash python3 tests/pty/perf.py | grep -E "latency|lost|source|prompt"
'
