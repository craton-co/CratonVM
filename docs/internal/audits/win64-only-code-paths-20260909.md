# Which Win64-only code paths a Linux-only test run cannot see

**Date:** 2026-09-09. **Prompted by**
`docs/internal/fixed-bugs/ir-osr-entry-miscompiles-a-spliced-merge-FIXED-20260909.md`,
whose second half was a miscompile that five fixtures on the Linux build box
could not reproduce and that reproduced on the first run on Windows.

The question this answers is not "what differs between the platforms" — that is
a grep — but **"where does the Windows arm carry behaviour that no test on a
System V host can reach?"** Those are the places a green suite on the build box
is not evidence.

## The classification

Three kinds, and only the first is dangerous.

### 1. The feature is switched OFF on System V, so its consumers are dead code

The Windows-shaped state is produced by the platform and by nothing else, so no
Linux test can construct it. **Two sites in the whole JIT.**

| site | what is unreachable on Linux |
|---|---|
| `x64.rs`, `Compiler::new` | float/double locals in XMM8–15, **single-pass tier** |
| `regalloc.rs`, `IR_PROLOGUE_SAVED` | XMM residency, **optimizing tier** |

The second is the one the page above fixed. The first is larger: it is the tier
that compiles every method, and `xmm_for_local` feeds **fifteen emission
sites** — ten `dload`/`dstore`/`fload`/`fstore`/accumulator arms in
`bytecode_walk`, the register-parameter load, the stack-parameter load and the
XMM zero-init in `frames`, plus the deopt description in `deopt_stubs`. With
`#[cfg]` at the gate, every one of them was dead code on the host that runs the
suite.

**Closed 2026-09-09** by `xmm_local_homes_enabled()` — a function defaulting to
`cfg!(windows)` with a test-only thread-local override, in the idiom this crate
already uses five times (`LsForce`, `OsrEntryForce`, `DeoptRegsForce`,
`CarryForce`, `AluImmForce`). It is **emission-only**: the default is a
correctness rule (System V makes every XMM caller-saved;
`MonotonicLongValues.Builder.pack` lost a local across an `invokespecial` and
corrupted Lucene's document map), and a thread-local does not change the ABI.
Two A/B tests now cover the prologue's three sites and the ten bytecode arms,
and both fail on Linux when the override is removed.

### 2. Parallel implementations, each exercised on its own platform

Both arms are live and ordinary tests reach whichever is compiled. Lower risk —
but still only validated on a host that runs them.

* `xt_root_scan.rs` — a 627-line Windows `SuspendThread` takeover against a
  Linux SIGUSR2 rendezvous, for cross-thread JIT-frame root scanning. Its three
  tests are deliberately **not** platform-gated, so they exercise whichever
  `imp` is built. That is the right pattern; coverage of the takeover machinery
  itself is thin on both.
* `licm.rs` — inline TLS through `gs:` (Windows) and `fs:` (Linux), with
  separate displacement probes. `inline_rbp_tls_segment_prefix()` is a clean
  `const fn` over the one byte that differs.
* `STACK_ARG_BASE` / `stack_arg_block_size` — Win64's 32-byte shadow space, so
  a caller's stack arguments begin at `[rbp+0x30]` rather than `[rbp+0x10]`. A
  wrong constant here reads arguments from the wrong place, which any Windows
  test with enough arguments would catch loudly.
* `platform.rs` — icache flush and W^X, with a `#[cfg(windows)] #[test]`
  smoke test of its own.

### 3. The divergence is asserted, in a test that knows about it

Exactly one family, and it is the pattern the rest should copy:

```rust
assert_eq!(vector_pool_is_encodable(8, &[]), !cfg!(windows));
```

`vector_pool_is_encodable` is *inverted* relative to everything above — Windows
is the RESTRICTIVE arm, refusing a vector pool the caller's prologue did not
save. Refusing is the correct answer rather than a missed optimization, and the
test pins both answers.

## Two numbers worth keeping

* **8 Windows-only tests exist in the entire workspace** — path handling,
  encodings, proxy env vars, one icache smoke test. **None are codegen.**
* In the whole `jit` crate, `cfg!(windows)` appears in **2** test assertions,
  both in `vec_emit.rs`.

Where the pattern *was* already done right, it was done by fabricating the
Windows-shaped state rather than by gating the test: the single-pass OSR
trampoline's XMM seeding is covered by fixtures that set
`osr_xmm_assignments = Some(vec![Some(1), ..])` directly on a `CompiledMethod`,
and `frame_value_for_slot(None, Some(9), 16, false)` unit-tests the deopt
provenance of an XMM home. Both run on Linux. The single-pass gate defeated
that approach only because it sat *inside the constructor*, between the fixture
and the emitter — which is exactly what the change above moves out of the way.

## What running the suite on Windows actually found

18 233 tests pass; 2 targets fail. **Neither is a platform gap** — both were
checked against Linux and fail there identically:

| failure | verdict |
|---|---|
| `no_new_test_only_public_api` | 297 offenders against a baseline of 299. A *ratchet* asking to be lowered — the test says so itself. Fails the same on Linux. Someone removed two offenders without lowering `BASELINE_OFFENDERS`. |
| `vthread_gc_stress_completes` | Times out after 120 s. Fails the same on Linux **once a binary exists**. |

That second row is its own small finding. The probe resolves `cratonvm` from
`CRATONVM_BIN` or the cargo target directory and **passes when it finds
neither** — so on a machine that has not built the CLI, all four of its tests
report `ok` in 0.00 s. It read as Windows-only until a debug `cratonvm` was
built on the Linux box, at which point it timed out there too. A test that
passes by doing nothing is the same hazard as a platform-gated one, arrived at
from a different direction.

## What is still owed

Nothing in category 1 — it is empty now. In category 2, `xt_root_scan`'s
takeover machinery is the thinnest coverage in the list on **both** platforms,
and it is GC correctness; that is a coverage question rather than a platform
one, and it is not addressed here.

The honest summary of the whole exercise: the gap was real, it was one site,
and the reason it survived is that the gate sat where no test could reach
around it.
