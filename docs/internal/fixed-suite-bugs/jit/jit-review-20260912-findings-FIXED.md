# FIXED: the 2026-09-12 JIT code review, finding by finding

**Status: 87 FIXED, 4 PARTIAL, 6 OPEN** across 97 findings. The ledger has
95 lines, because #10/#11 and #18/#19 each share one fix and one line. Every
commit is on `fix/jit-review-20260912`, merged into `dev`. Each PARTIAL and
OPEN finding names the known-issue record that tracks what is left.

## What the review covered

The single-pass x86-64 tier, the IR tier (builder → optimizer → scheduler →
linear scan → lowering), the AArch64 backend, deoptimization and OSR, tiering,
the code cache and inline caches, the helper ABI, and the VM glue. Findings
were ranked critical, high, medium, low, and architecture.

## How the work was verified

- `cargo test -p cratonvm-jit --lib`: grew from 2,402 to over 2,420 tests, all
  passing.
- `cargo test -p cratonvm-jit-api --lib`.
- `cargo test -p cratonvm-difftest`: the new path-gate and `fuzz-jit` harness.
- `cargo test -p cratonvm-vm --lib -- jit deopt tier osr cache helper_guard`
  and `--test no_test_only_public_api`.
- `cargo test -p cratonvm-types`: flag surface, flag docs, doc numeric claims,
  doc citation paths.
- `cargo clippy -p cratonvm-jit --tests`: clean under the crate's new
  `#![deny(clippy::undocumented_unsafe_blocks)]`.

Nothing in this ledger was run on a release binary against the JDK oracle.
The CI job `difftest-jit-paths` and the nightly `fuzz-jit` workflow added by
the review are the gate that does that.

## Ledger

### Critical

| # | Finding | Status |
|---|---|---|
| 0 | SIMD pre-header entered with `i > n` runs ~2^29 chunks past the array | FIXED 3f2b406d8 |
| 1 | AArch64 scratch spills keyed by register | FIXED 2fa01e1c2, b903ebb1a |
| 2 | AArch64 `freturn`/`dreturn` emit a bare RET | FIXED 2fa01e1c2 |

### High

| # | Finding | Status |
|---|---|---|
| 3 | f2i/f2l/d2i/d2l overwrite a pending XMM0 value | FIXED 3f2b406d8 |
| 4 | CMOV min/max peephole absorbs branch targets | FIXED 3f2b406d8 |
| 5 | dmul-by-2.0 reduction on a merge point | FIXED 3f2b406d8 |
| 6 | Win64 scratch XMM6/XMM7 never saved | FIXED 3f2b406d8 |
| 7 | `double[]` SIMD sum reorders FP additions | FIXED 5d977965e (lowering retired) |
| 8 | `int[]` SIMD sum wraps at 32 bits into a long | FIXED 3f2b406d8 |
| 9 | aaload LICM reuses an element across calls | FIXED 3f2b406d8 |
| 10, 11 | Null-check elimination misses switch/handler edges and reads the textual predecessor | FIXED 67ab0262c |
| 12 | IR LICM hoists a throwing Load | FIXED 58bb7cfec |
| 13 | IR simplifier folds FP `x - x` to integer 0 | FIXED 58bb7cfec |
| 14 | Loop-header phi stays Ref for a primitive back edge | FIXED 58bb7cfec |
| 15 | IR MonitorExit writes 1 into the locked object | FIXED 58bb7cfec |
| 16 | Linear-scan clobber model omits aastore/monitor helpers | FIXED 58bb7cfec |
| 17 | Scalar replacement through phi(new, null/checkcast) | FIXED 58bb7cfec |
| 18, 19 | PIC installs pair ids with the wrong entry; ways rewritten under four loads | FIXED c74ac7e5f |
| 20 | Retired code never freed while a thread is parked in compiled code | FIXED 66f629c9f, b953885df |
| 21 | A compile panic kills the only compile thread | FIXED f960bed8f |
| 22 | Every deopt counts toward a permanent C2 ban | FIXED f960bed8f |
| 23 | `Arrays.sort` intrinsic is O(n²) with no cap | FIXED 58bb7cfec (heapsort past 47) |
| 24 | getstatic/putstatic helpers run `<clinit>` without a safepoint | FIXED 58bb7cfec |
| 25 | Bridge argument copies unrooted across GC | FIXED 45c07691e |
| 26 | Scalar-replaced materialization allocates with unrooted references | FIXED 45c07691e |
| 27 | AArch64 switches compare a clobbered key | FIXED (a64 merge f947a8973) |
| 28 | AArch64 long/double/float parameters mis-homed | FIXED (a64 merge f947a8973) |
| 29 | AArch64 FP semantics (fcmpg NaN, float as double, d2i width) | FIXED 2fa01e1c2 |

### Medium

| # | Finding | Status |
|---|---|---|
| 30 | AArch64 JIT publishes code by default | FIXED 5e9387cb0 (`CRATONVM_JIT_ARM64`, default off) |
| 31 | FreeBSD/macOS use Linux's `MAP_ANONYMOUS` | FIXED aed9d9603 |
| 32 | macOS `MAP_JIT` write protection; non-standard AArch64 frame record | FIXED aed9d9603, 2fa01e1c2 |
| 33 | Win64 XMM8–15 saved as 64-bit values | FIXED 58bb7cfec |
| 34 | Constant-compare peephole drops the back-edge poll | FIXED 58bb7cfec |
| 35 | SIMD pre-headers run an unbounded span without a poll | FIXED 45c07691e |
| 36 | `jit_service_callee_deopt` after the shadow publication popped | FIXED 45c07691e |
| 37 | Uncommon-trap stub passes RBP as `SharedVm*` | FIXED 58bb7cfec |
| 38 | IR unroll simulates an int IV in i64 | FIXED 58bb7cfec |
| 39 | SCEV trip-count floor ignores extra exits and re-entry | FIXED 3f771684f |
| 40 | IR long Cmp uses a 32-bit CMP | FIXED 58bb7cfec |
| 41 | Every synchronized-block method refused after full emission | FIXED 58bb7cfec |
| 42 | A lowering refusal publishes an accepted verdict | FIXED 45c07691e |
| 43 | Bottom-tested loops refuse the IR build | OPEN `ir-builder-refuses-rotated-loops-20260912.md` |
| 44 | `ir_compatible` admits opcodes the builder refuses | FIXED f5329cdd5, 9b8cbce45 |
| 45 | `ir_evidence` entries leak on an IR bail | FIXED add815189 |
| 46 | IR frame states carry no monitor stack | OPEN `ir-frame-states-carry-no-monitor-stack-20260912.md` |
| 47 | Lock elision disables the monitor precise-resume gate | FIXED 347220a6b |
| 48 | Array scalar replacement forwards an un-narrowed int | FIXED 347220a6b |
| 49 | Allocation elision never fires by default | OPEN `allocation-elision-never-fires-by-default-20260912.md` |
| 50 | Dependencies never re-validated; CHA registry dead | FIXED 15d8f89af |
| 51 | Invalidation clears ICs before unpublishing | FIXED c74ac7e5f, 15d8f89af |
| 52 | O(n) copy-on-write registries rebuilt per change | FIXED fe92eb53b |
| 53 | Two code-retirement systems | FIXED d7f658e9e, 66f629c9f |
| 54 | 250 ms C2 budget on wall time, for failures and OSR | FIXED f960bed8f |
| 55 | Tier state, OSR denials, unload keyed by class name | PARTIAL f960bed8f, c9cf67009; `jit-verdicts-are-keyed-by-name-not-loader-20260912.md` |
| 56 | One failed background OSR compile bans OSR forever | FIXED f960bed8f |
| 57 | Branch-profile window arm/disarm unbalanced | FIXED f960bed8f |
| 58 | Never-compiling methods lock a global mutex every 64 calls | FIXED f960bed8f |
| 59 | CompilationBroker fed but never read | FIXED 28214a21e (deleted) |
| 60 | JIT `Integer.valueOf` ignores `IntegerCache.high` | FIXED 45c07691e |
| 61 | Uncommon trap charges the interpreted caller | FIXED 45c07691e |
| 62 | instanceof-only method takes the thread-less entry | FIXED 45c07691e |
| 63 | A panic in an `extern "C"` helper aborts; MSRV wrong | PARTIAL 347220a6b (MSRV), panic merge 4749bab18, 1925cdabb; `jit-leaf-helper-panics-still-abort-20260912.md` |
| 64 | Optimizing-tier OSR artifacts rebuilt on every trigger | FIXED (cached through `put_osr`/`get_osr`, refusals memoized in the bridge) |
| 65 | OSR trampoline drifts from the prologue | FIXED add815189 |
| 66 | "Permanent" OSR reject memo and bail list never cleared | FIXED 24e98eed4, 743220e11 |

### Architecture

| # | Finding | Status |
|---|---|---|
| 67 | CI never runs the JIT's differential modes | FIXED ed2b278b2, 3432a0d6c, f1afaaf37 (merge 8e9372d97) |
| 68 | Two optimizing front ends, duplicated analyses | OPEN `two-optimizing-front-ends-duplicate-bytecode-analyses-20260912.md` |
| 69 | God functions and request side channels | OPEN `jit-god-functions-and-request-side-channels-20260912.md` |
| 70 | Presence-based flag parsing makes `=0` mean on | FIXED 0b3c07451 (merge 092d601fd); `jit-presence-only-flag-reads-FIXED.md` |
| 71 | 620 process-global statics, some of them compatibility state | FIXED merge 708cc7866; `jit-compatibility-and-despec-state-per-vm-FIXED.md` |
| 72 | Undocumented unsafe allowed | FIXED 0243c30ad |
| 73 | Core docs several defaults out of date | FIXED docs merge ebbda0506, bf955d304 |
| 74 | No perf map, jitdump or GDB JIT interface | FIXED perfmap merge fecf08937 |

### Low

| # | Finding | Status |
|---|---|---|
| 75 | frem/drem have no single-pass arm | FIXED 7335f4c73 |
| 76 | instanceof has no inline class-id fast path | FIXED c37371e2b (merge 9147ff17f) |
| 77 | iushr breaks the sign-extended-int invariant | FIXED 7335f4c73 |
| 78 | lookupswitch assumes sorted keys | FIXED 7335f4c73 |
| 79 | Bail-list keys XOR field hashes | FIXED 24e98eed4 |
| 80 | Intrinsics: layout twice, exact CP class, env read per call | PARTIAL 7335f4c73, e9838de83; `atomic-intrinsics-miss-subclass-call-sites-20260912.md` |
| 81 | Dead intrinsic variants and hand descriptor walkers | FIXED 7335f4c73, c9cf67009, efcc72e7e |
| 82 | Unresolved checkcast returns null | FIXED 7335f4c73 |
| 83 | Helper ABI self-check skips callable slots | FIXED 7335f4c73 |
| 84 | Boolean returns never masked | FIXED 7335f4c73 (bridge), 123783c3d (both tiers and inliners) |
| 85 | `jit_anewarray_object` missed its siblings' fixes | FIXED 7335f4c73 |
| 86 | Peer takeover root scan reads GPRs only | FIXED 7335f4c73 (no-oops-in-XMM invariant documented) |
| 87 | OSR trampoline cache keyed by address only | FIXED add815189 |
| 88 | Resume tolerates Unsupported locals on a wrong argument | OPEN `jit-resume-tolerates-unsupported-locals-without-liveness-20260912.md` |
| 89 | Scheduler places nodes in id order | FIXED 7335f4c73, merge 44f2aedf4 |
| 90 | Quadratic scheduler and lowering scans | PARTIAL merge 44f2aedf4; `ir-scheduler-and-lowering-remaining-quadratic-scans-20260912.md` |
| 91 | DCE roots omit throwing ops | FIXED 9b8cbce45 |
| 92 | Unroll refused on any side effect after the loop | FIXED 5092d22aa |
| 93 | Dead loop machinery costs compile time | FIXED 7335f4c73; unrouted emitter: `vec-emit-vector-loop-emitter-is-not-routed-20260912.md` |
| 94 | `handler_has_unsafe_local_read` does not decode wide | FIXED 9b8cbce45 |
| 95 | Dead tiering APIs; receiver table freezes | FIXED f960bed8f |
| 96 | One compile thread serves C1 behind C2 and OSR | FIXED f960bed8f (two lanes) |

## Defects found while landing the fixes

These are not review findings, but the review branch hit them and fixed them:

- **Parked-thread code reclamation.** The lazily published region snapshot let
  a parked thread's stack summary miss a freshly published body, which is a
  use-after-free window. `collect_code_buffers_in_band` now catches the
  snapshot up first (b953885df).
- **Compile ids leaked by failed compiles.** An id reserved by a compile that
  then bailed was never released. `CompileIdReservation` fixes it (bbd643986).
- **Withdrawn lambda thunks not retired.** They were dropped without being
  marked retired or passing through the retirement queue (bbd643986).
- **Debugger symbols.** They are withdrawn for the whole unmapped buffer range
  (fecf08937).
- **Panic containment.** It was narrowed to helpers whose failure answer is
  sound at their call site. A crash report now names contained helper panics
  (1925cdabb).
- **Differential fuzzer.** The fp-compare generator bound a branch frame
  without the operand already on the stack, and Java 5 classes named
  `StackMapTable` (fecf08937).
