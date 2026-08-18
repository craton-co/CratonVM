# `jit_invoke_virtual_mic` held two `&mut JvmThread` at once, and used the invalidated one

**Status: FIXED 2026-08-18** on `fix/jit-thread-mut-sibling-borrow-20260818`,
branched from `dev` @`36433bf5d`.

A `debug_assert!` in `jit_thread_mut` had been claiming this for as long as
anyone ran a dev-profile build against a real workload. It was right.

## How it surfaced

Not from a bug report. A dev-profile build was wanted for an unrelated
*counting* measurement — the release build was blocked by Windows page-file
exhaustion — and the debug binary would not run an ordinary BouncyCastle
workload at all:

```
thread 'main-vm' panicked at vm/src/jit/helpers.rs:1338:
jit_thread_mut: aliasing &mut JvmThread borrow detected (a prior JitThreadGuard
is still live at the SAME JIT nesting level — this is a genuine sibling
fabrication, not a re-entry)
```

Twice per run, during boot, and it reproduced with every diagnostic flag off.

## Finding the pair

The assertion names neither side, and `vm/src/jit/helpers.rs` has **58**
`jit_thread_mut()` call sites, so reading was not going to do it. The instrument
is one thread-local: record a `Backtrace` when a borrow is taken, print it
beside the current one when the flag trips. Behind
`CRATONVM_DBG_JIT_BORROW_SITES=1`, because capturing a backtrace on every borrow
costs far more than the borrow.

One run:

```
PRIOR : jit_invoke_virtual_mic -> jit_thread_mut                    helpers.rs:14361
SECOND: jit_invoke_virtual_mic -> try_jit_site_cached_native_dispatch
                               -> jit_thread_mut                    helpers.rs:11862
```

Both sides are inside **the same function**.

## The defect

`jit_invoke_virtual_mic` binds

```rust
let (thread, _jit_thread_guard) = match jit_thread_mut() { ... };
```

and holds `_jit_thread_guard` for the rest of the body. Roughly a hundred lines
later it calls `try_jit_site_cached_native_dispatch`, whose leaf-native arm did
its own

```rust
let (thread, _guard) = jit_thread_mut()?;
```

There is no `set_jit_thread` between them, so the inner `&mut *ptr` is not a
child reborrow of the outer — it is a sibling derived independently from the
same raw pointer. Two live `&mut JvmThread`.

**And the outer one is used afterwards.** Counting from the outer borrow, the
call to the site-native helper is at relative line 106; `thread` is then passed
as `&mut` to `safe_native_call` (235), `handle_jit_dispatch_error` (243, 341,
384, 411), `call_matcher_native_raw` (299) and five more sites, all after 106.
So this is not the benign "held but never touched again" shape: under
Stacked/Tree Borrows the inner `&mut` invalidates the outer, and every one of
those later uses is unsound. `&mut` parameters carry `noalias`.

The sibling caller `jit_invoke_dispatch` (`:10938`) was **already correct** —
its nearest borrow is scoped to an `if let` that closes before the call.

## Does release have the same overlap?

Yes. Only the *detection* is `#[cfg(debug_assertions)]`:

* `JIT_THREAD_BORROWED`, `JitThreadGuard::_private`, `suspend_jit_borrow` and
  the `debug_assert!` are all gated;
* neither `jit_thread_mut()` call, and neither `&mut *ptr`, is gated at all.

Release compiled the same two derivations and simply did not look. Confirmed by
measurement rather than by reading the attributes: the borrow tracking was
compiled into a **release** build behind a one-off `prove-borrow` feature, with
the assertion replaced by a counter so a run reports every occurrence instead
of dying at the first.

Release build, pre-fix code, `Sha256Kernel`:

| iterations | aliasing overlaps counted |
|---:|---:|
| 300 | 1 |
| 20 000 | 58 000 |
| 200 000 | **598 000** |

**It scales with the workload.** This is not a boot-time curiosity that happens
twice while `BigInteger` initialises — that is merely where the debug build
died first, because a `debug_assert!` aborts on the first one. It is roughly
three per iteration on a hot virtual-dispatch path, six hundred thousand times
in one ordinary run of a release binary, every one of them a use of a `&mut`
that a sibling `&mut` had invalidated.

## The fix

Pass the caller's borrow down instead of re-deriving one — what the borrow
checker would have forced if the raw pointer were not in the way.
`try_jit_site_cached_native_dispatch` now takes `thread: &mut JvmThread`; the
MIC caller hands over the one it already holds, and `jit_invoke_dispatch`
acquires one for the duration of the call (its `?`-on-`None` behaviour is
preserved: no JIT thread installed means the call is skipped, which is what the
callee's `jit_thread_mut()?` did).

Also fixed in the same change, in the new instrument: `JitThreadScope` suspended
the borrow FLAG across a nested JIT entry but not the recorded SITE, so the
nested case restored `flag = true` over `site = None` and a later trip would
have reported "no site recorded" with capture switched on — an instrument that
goes quiet exactly where it is needed.

## Verification

| | before | after |
|---|---|---|
| `Sha256Kernel 300`, dev profile | 2 panics, run dies (exit 127) | 0, completes, `checksum=1500` |
| `Sha256Kernel 200000`, RELEASE + `prove-borrow` | **598 000 overlaps** | see below |
| `Dup2X2Probe 2000`, `Rc5Drive 200`, dev profile | — | 0 overlaps, correct results |
| `cratonvm-vm --lib` | 2565 passed | 2565 passed |
| `cratonvm-jit --lib` | 2031 passed | 2031 passed |

The two extra probes are evidence that no *other* overlap surfaced on those
paths — not evidence about the fix, since they were never shown to fail before
it.

**On the release GREEN arm, stated precisely.** The 598 000 figure is the RED
arm and is what answers "does release have the same overlap". The matching
GREEN arm — same release binary, same `prove-borrow` counter, fix applied —
did not build: `cratonvm-cli` failed twice more on this host for the
page-file reason described in
known-issues/jit/every-jit-getfield-takes-the-helper-because-the-guarded-inline-check-always-fails-20260817.md.
The fix's effect is therefore demonstrated in the **dev** profile (2 → 0) and
by construction — the inner `jit_thread_mut()` no longer exists, so there is no
second borrow left to count — but the release zero is not measured, and this
page does not claim it.

**One test failure, investigated and not ours.**
`jit::code_cache_lifecycle::tests::the_production_quiescence_signal_is_the_global_jit_depth`
failed once during a full `cargo test -p cratonvm-vm --tests`, and passes 3/3
in isolation and in two full `--lib` runs of the same tree.

The first explanation reached for was "the lib binary runs in parallel with 169
integration binaries" — which is **wrong**, because those are separate
processes and cannot perturb each other's statics. The real exposure is
intra-binary and timing-dependent: the test does
`conservative_roots::push_jit_entry_at(sp)` and then asserts
`any_thread_in_jit()`, with a comment explaining that the anchor address is
chosen "so the entry is not pruned as provably returned while we hold it" — a
property that depends on where OTHER threads in the same binary happen to have
their stack pointers. Under heavy machine load that race widens.

Nothing in this change touches `conservative_roots` or JIT-entry tracking; the
change removes one `jit_thread_mut()` call and threads a reference through a
parameter.

## Why no suite caught it

The assertion has been in the tree and correct for a long time, and every
dev-profile run of a real-JDK workload trips it. It went unnoticed because
nothing routinely runs the VM binary in a dev profile against a real JDK: the
169 integration tests in `vm/tests/` are built in debug but do not drive
`jit_invoke_virtual_mic` through its site-native-cache arm, and every
measurement and suite run uses `--release`, where the check is compiled out.

**A debug-only assertion is only as good as the debug-profile runs nobody
does.** The cheapest guard against the next one is a periodic dev-profile run
of one real workload; it costs a two-minute build and it found this in the
first attempt.

## The transferable part

**An assertion that names the violation but not the participants is one
thread-local short of a diagnosis.** This one had been printing a precise,
confident, correct message for months, and the message was still not actionable:
58 candidate call sites, and no way to tell which two were live. Recording where
the live borrow was taken turned an unactionable panic into a two-line answer on
the first run.

And the corollary that made it worth chasing: **`#[cfg(debug_assertions)]` on
the CHECK says nothing about the CODE.** The natural reading of "debug-only
assertion" is "debug-only problem". Here the guarded thing was the observation;
the aliasing was unconditional.
