# SCIM filter parser no longer requires a trailing `AND` clause — FIXED

Status: FIXED — validated on the dedicated worktree; see validation below.

Date fixed: 2026-07-12

## Resolution

CratonVM now installs an ANTLR-runtime override for `DefaultErrorStrategy.sync(Parser)` for both the standard
`org/antlr/v4/runtime` and Groovy-shaded `groovyjarjarantlr4/v4/runtime` packages. The override makes the
recovery-only sync hook side-effect-free; ANTLR adaptive prediction remains the authoritative parser decision.

## Root cause

This was a deeper interpreter/runtime divergence, not a SCIM grammar defect. ANTLR `LL1Analyzer` should include
its `EPSILON` sentinel (`-2`) in recovery lookahead at a left-recursive loop exit. Under CratonVM, the generated
lookahead instead contained only `AND`. `DefaultErrorStrategy.sync` consequently treated EOF, `)`, and `]` as a
missing `AND`, before `ParserATNSimulator.adaptivePredict` could correctly select the zero-repetition exit.

The behavior persisted with all CratonVM ANTLR native ATN/prediction-context registrations disabled, proving the
underlying issue is in interpreter/runtime execution of the Java ANTLR path rather than the prior Rust ATN
implementation. Skipping only `sync` restored correct parse behavior, including the negative-input checks.

## Validation

On the Azure Linux host with real JDK 25 and a fresh SCIM fixture compilation:

- `cargo test --release -p cratonvm-native-builtins antlr_prediction_context_intrinsics_are_registered --lib` passed.
- HotSpot baseline: `FilterUtilsTest` 39/39 passed.
- CratonVM fixed binary, JIT on: `FilterUtilsTest` 39/39 passed.
- CratonVM fixed binary, `--nojit`: `FilterUtilsTest` 39/39 passed.

The focused regression exercises single comparisons, AND/OR combinations, grouped expressions, value paths,
valid/invalid null comparisons, bounds/depth validation, and malformed syntax.
