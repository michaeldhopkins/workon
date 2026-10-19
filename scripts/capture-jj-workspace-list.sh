#!/bin/sh
# Regenerates tests/fixtures/jj/<version>/ from the jj on PATH.
# The ignored test uses LIST_TEMPLATE, so the fixture and the parser share one template.
# jj 0.46.0 is not installed here. This script does not install a jj.
set -eu
cd "$(dirname "$0")/.."
cargo test --locked --lib registered::tests::capture_jj_workspace_list -- --ignored --exact --nocapture
