#!/usr/bin/env bash
# TOK10-1 (JIT round 19 proposal wave 11, lane lv11): run the jit crate's own
# unit tests with the token-read guard in its STRICT test mode.
#
# `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD=strict` turns the guard on for every
# compile the tests make AND, in a test build only, turns a refusal into a
# panic that names the funnel (`file:line`), the reader and the token-only
# input (`Lowerer::token_read_check`). So a green run is the CI fact "no
# lowering arm read an operand through a memory-token slot on any of the
# crate's hand-built or javac-shaped graphs"; a red test is a defect page with
# the printed `file:line`. The two tests that latch a refusal on purpose pin
# the guard to `1` and stay green.
#
#   tools/jit/token-read-guard-lib-tests.sh [extra cargo test args...]
#
# Respects CARGO_TARGET_DIR. Pass a test-name filter to narrow it.
set -euo pipefail
export CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD=strict
exec cargo test -p cratonvm-jit --lib "$@"
