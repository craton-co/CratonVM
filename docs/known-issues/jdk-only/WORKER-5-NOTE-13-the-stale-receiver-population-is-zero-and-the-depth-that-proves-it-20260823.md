# WORKER-5 NOTE 13 — the stale-receiver population is ZERO, and the depth that had been hiding a third of it

**Status: MEASURED and FIXED.** Lane WORKER-5, 2026-08-23. Closes
`WORKER-5-NOTE-12` N1, N2 and N5, and `WORKER-5-NOTE-11` N3.

`WORKER-5-NOTE-12` swept all seven native crates, fixed the
`native-collections` instances, and **handed over 18 findings unfixed** on the
grounds that a 45-site migration across another lane's 700k lines would be
unreviewable. That reasoning was wrong in one respect: the migration is
**rustc-driven**, so it is not a reading exercise. Every site the compiler does
not reject is a site that did not need changing.

The population is now **0 fn / 0 sites at the fixpoint depth**, across all
seven crates.

---

## 1. N5 first, because it changed what "everything" meant

NOTE-12 shipped a **depth-2** baseline and listed the depth bound as its first
limitation. Answering N5 required running to convergence, and the tool could
not: allocation reachability was computed by building one
`a|b|c|...`-alternation regex per round and running it over all 16,769 bodies.
At depth 3 the frontier is ~6,300 names and **it did not finish in ten
minutes**.

Rewritten to tokenise each body's callee names once and intersect sets — the
same `(?<![a-z_0-9.])name\s*\(` pattern the alternation used, so the two agree
by construction. Held to that by **reproducing the committed depth-1 and
depth-2 numbers exactly** (5147/130/17/44 and 6104/145/18/45) before trusting
anything new from it. The whole scan is now ~9s at any depth.

| depth | allocating | matching the shape | with reusing sites |
|---:|---:|---:|---:|
| 1 | 5,147 | 130 | 17 fn / 44 |
| 2 | 6,104 | 145 | 18 fn / 45 |
| 3 | 6,348 | 154 | 18 fn / 45 |
| 4 | 6,476 | 164 | 18 fn / 45 |
| 5 | 6,528 | 171 | **21 fn / 49** |
| 6 | **6,539** | **171** | **21 fn / 49** |
| 8, 12 | 6,541 | 171 | 21 fn / 49 |

**The fixpoint is depth 6**, and the default is now 6 rather than 1. Depths 3
and 4 are the trap: they look like convergence and are not.

### 1.1 The crate I had called clean, for the third time

Of the three that only appear at depth 5, one is **`map_resize`**, in
`native-collections` — the crate NOTE-12 §3 called clean at depth 1 and NOTE-12
§7 called *genuinely* clean at depth 2.

It is the same species as the two before it. `map_resize_inner` **already knows
its receiver moves**: it pins `this`, re-reads it across `new_ref_array`,
re-reads it again after the rehash (whose own comment, `HIB-MAPRESIZE-STALE.1`,
explains that the "allocation-free" claim above it was false), and then
**dropped that refreshed value at its closing brace**. Of its three call sites
**one** re-read through its own pin and **two did not**, going straight into
`map_state(ctx, backing_map)`.

That is three findings in one crate, each invisible to the depth the previous
record had measured. "Zero at depth N" is worth exactly N.

## 2. The migration

24 funnels (23 by script, `map_resize` by hand), 70 call sites.

* Each funnel's original body keeps its by-value receiver and is renamed
  `*_body`; a wrapper of the original name takes `&mut ObjectRef`, pins, calls,
  writes the refreshed reference back, and unpins. `unpin_native_roots(base)`
  releases everything from `base` onward, so the wrapper's unpin is also a
  backstop; **no converted body releases a pin it did not take** (checked — if
  one did, the wrapper's own handle would go out of range and `read_native_pin`
  would quietly answer the stale fallback).
* `&mut` made every unconverted caller a compile error. The 49 that resulted
  were all `E0596`, and all were fixed by applying **rustc's own
  MachineApplicable suggestions** rather than by hand — any diagnostic that was
  not `E0596` was set up to stop the loop, and none appeared.
* Nine call sites passed a receiver borrowed out of `args` (`*this`, `*k`) and
  needed judgment, not a script. Seven were the last statement in their block —
  no post-call use, so no defect — and took a real local (`let mut this =
  *this;`) rather than a throwaway `&mut { *this }`, so a future appended
  statement inherits the refresh instead of silently reintroducing the bug.
  **The two in `jca/signature.rs` were live**: after
  `register_rsa_{priv,pub}_*_material` the code re-reads the key AND stores it
  into `SIG_OFF_KEYOBJ` — a **GC-scanned slot**, so a stale reference parked
  there is durable rather than transient.

## 3. `cargo check -p <crate>` is not the build

`cargo check -p cratonvm-native-builtins` was clean. The release build of
`cratonvm-cli` then failed with **two more `E0596`s in `jmx.rs`**, in code the
per-crate check's default feature resolution never compiled.

A per-crate check is a fast filter, not the gate. Checking through
`-p cratonvm-cli` reproduces the binary's feature set without paying for LTO,
and that is what the remaining rounds used.

## 4. The gate

Baseline **empty**; default depth 6; `--selftest` green.

An empty baseline is the one most in need of a positive control, because a
detector that has silently stopped matching anything produces a byte-identical
clean run. So the failure path was re-exercised **at the zero baseline** — a
synthetic allocating funnel plus a caller that reuses the receiver, appended to
`native-io/src/lib.rs`:

```text
  WITH call sites that reuse the receiver: 1 fn(s), 1 site(s)
  TRIPPED: w5_canary_funnel now has 1 reusing site(s) (baseline 0)   rc=1
  [restored]                                                        rc=0
  --selftest                                                        rc=0
```

`.github/workflows/stale-receiver-audit.yml` runs `--selftest` first, then the
audit, on every push. No build, no JDK, ~9s.

## 4a. The gates, and the two failures that WERE mine

Corpus arms, unchanged from the pre-change baseline:

```text
  SUITE=all CRATONVM_ARGS=--jdk-only    106 / 107      RJdkJmx  (NOTE-11 §5)
  SUITE=all                             107 / 107
  SUITE=core                             67 /  67
```

The arms are not the gates, so `cargo test --workspace --no-fail-fast` was run
on this tree AND on pristine `af3f6c943`. **24 failures each, 21 identical.**

**Two were mine, and both were the same species.** `native-builtins --lib` had
exactly two, both **source-witness** tests that grep the working tree for
literal call text that this migration changed:

* `every_thread_constructor_captures_inheritable_thread_locals` searched nine
  `Thread` ctor bodies for `capture_inheritable_tl_at_construction(ctx, this)`;
  now `(ctx, &mut this)`. Kept EXACT rather than loosened to a name match —
  that each body passes its OWN `this` is what the witness is for.
* `the_lazy_exchange_guard_is_a_presence_test_not_a_liveness_test` scanned
  `fn https_ensure_exchanged(` for a `contains_key` presence test; that guard
  now lives in the `_body` half, so it scans there. The `expect` keeps a
  further rename loud.

Both fixed; `-p cratonvm-native-builtins --lib` is rc=0. The other 22 names
were swept for string-literal references across `*.rs`/`*.md`/`*.sh`: the
remaining hits are prose and one debug format string.

### 4a.1 The other six differences were the HOST, not the change

Three tests failed only here and three only on the control. None is anywhere
near a receiver refresh, and a migration that FIXES three JIT/attach-socket
tests has no mechanism. The host's 15-minute load average was **134 on 8
cores** while those runs executed.

Re-running each of the four distinct tests 5x per tree on a quiet host:

```text
  w5-final  a_heap_that_publishes_no_movable_bounds_cannot_prove_coverage  5/5 pass
  w5-final  a_capturing_sam_call_site_dispatches_through_a_thunk           5/5 pass
  w5-final  d15_attach_socket_threaddump_operation                         5/5 pass
  w5-final  recursive_compile_cycle_routes_parent_direct_call_...          5/5 pass
  w5-ctl    (the same four)                                                5/5 pass
```

**40/40.** Every one was a load artifact. `ci.yml` already carries a "GC suite
flake gate (repeat runs)" step, so the suite is known to have flaky members;
what is new is that a whole-workspace A/B taken under load manufactures BOTH
directions at once and reads as three regressions plus three gifts.

### 4a.2 Clippy could not see this change until three unrelated errors were fixed

`cargo clippy --workspace --all-targets -- -D warnings` fail-fasts. It died in
`cratonvm-types`, then in `cratonvm-jfr`/`cratonvm-reader` — all BEFORE any
crate this change touches, so its red said nothing about this code. Three
one-line fixes (`redundant_guards` x2, `byte_char_slices`), one of them in this
lane's own earlier `capped_var_verdict`, were needed just to reach the subject.

With those in, clippy reaches the changed crates. Seven error sites remain, in
`gc/`, `jit/`, `jit-cuda/`, `classloading/` and two `native-io` files — and
**none is a file this change edits** (checked mechanically against
`git diff --name-only`; the only `native-io` file touched here is `lib.rs`).
They reproduce on the control with the same three unblocking fixes applied.
They are NOT adopted: they are five other lanes' files.

Note for whoever re-runs this: under fail-fast, clippy's error SET is not a
stable population — which crates get linted depends on where it stopped, so
set-diffing two clippy runs is not valid. The file-overlap check is.

Feature gates, all green:

```text
  cargo check --workspace --features synthetic-jdk                       rc=0
  cargo check --all-targets --features synthetic-jdk -p vm -p builtins   rc=0
  cargo test -p cratonvm-native-builtins --lib --features synthetic-jdk  rc=0
```

## 5. What this does NOT establish

* **Still no reproduction for any of the 24.** They match the shape; not one is
  shown to produce a wrong answer. `resync_view_set` remains the standing
  example of a matching shape that could not be made to fail. The fixes are
  justified by the shape and by `RTreeRangeGc`, not by 24 failing tests.
* **Zero is a floor, not a proof.** Reachability is an intra-crate call graph
  keyed by NAME: a call through a trait object or a function pointer is
  invisible at any depth, and `ALLOC0` is a name list.
* **Only the receiver is modelled.** A NON-receiver local held across an
  allocating call has the identical hazard and is **not counted anywhere** —
  see §6 N1. `jca/signature.rs` has one in plain sight: `set_sig_keyid(ctx,
  this, kid)` runs after the allocating `register_*` call, and `this` is not
  the funnel's receiver.
* **The `&mut` conversion fixes the CALLER's view, not the body's.** Whether
  each body handles its own internal staleness correctly is a separate question
  this did not audit.
* **The three `populate_*_en` rows were `AMBIG`** — defined in both
  `locale_resources.rs` and `phases_late/text_intl.rs`. Both definitions were
  converted, so the ambiguity does not hide an unconverted one; but the crate
  label on such a row is still whichever definition was indexed last.

## 6. NOMINATIONS

* **N1 — the non-receiver family is unmeasured** (§5). Same hazard, no gate,
  and one instance is visible in the two `signature.rs` blocks this record
  touched. It will be a much larger and noisier population than 49; it needs
  its own ranking rule before it needs a baseline.
* **N2 — `CRATONVM_DBG_STALE_OBJREF` was never pointed at this.** The
  Generational backend already turns a stale read into a deterministic panic
  (`gc/src/gen_heap.rs`). An A/B of the corpus under it — pristine vs
  converted — is the cheapest route from "shape" to "reproduction", and would
  retire the caveat at the top of §5. It needs a pristine control binary, and
  the default collector is ZGC, so it needs `-XX:+UseGenerationalGC` too.
* **N3 — `RJdkJmx`** still needs the `jmx.rs` owner; control evidence in
  `WORKER-5-NOTE-11` §5.

---

### INDEX ROWS (for H0 to move into `INDEX.md`)

* `WORKER-5-NOTE-13` — the stale-receiver population is **0 fn / 0 sites** at
  the FIXPOINT depth (6), across all seven native crates. 24 funnels converted
  to `&mut ObjectRef`, 70 call sites, 49 compile errors resolved from rustc's
  own MachineApplicable suggestions. **Depth 3 and 4 look like convergence and
  are not**: depth 5 exposed three more, one of them `map_resize` in the crate
  two previous records had called clean. The audit tool could not answer its
  own question until its quadratic reachability was rewritten (verified by
  reproducing the old depth-1/2 numbers exactly). Also: `cargo check -p <crate>`
  passed code the release build rejected — two `E0596`s behind a feature the
  per-crate check does not resolve. MEASURED.
