# Testcontainers/Jackson stalls the Vert.x event loop on CratonVM — FIXED (and the filed diagnosis was wrong)

**Status:** FIXED / RETIRED (2026-08-12). Filed the same day against a Windows
(`C:\craton\CratonVM`) rerun of the hibernate-reactive suite, where 179 of 238
non-passed classes failed with

```
java.util.concurrent.TimeoutException: before(io.vertx.junit5.VertxTestContext) timed out after 120 seconds
```

and blamed a repeated JIT code-buffer bailout on
`BeanDeserializerBase._resolveManagedReferenceProperty` during Testcontainers'
Jackson-backed Docker-API deserialization.

Three things came out of re-testing it on the Azure Linux host
(`azureuser@20.80.105.49`) against `origin/dev` `1c4ce7d3a`:

1. **The filed failure does not reproduce**, including for the exact class the
   doc names.
2. **The doc's own open question is answered: the JIT bailout was not the
   cost** — it fires once per run and converges.
3. **Two genuine defects were found on the way to that answer and are fixed**,
   one of them the reason the bailout looked "repeated" at all, and one worth
   1.76x on every lambda and method-reference call CratonVM executes.

---

## 1. The filed failure does not reproduce

Everything below is `origin/dev` `1c4ce7d3a` with **no** fixes from this record
applied — i.e. the tree as the doc found it, minus the two blockers that landed
the same day (see "Why the numbers moved").

| measurement | CratonVM | HotSpot (JDK 25) |
|---|---:|---:|
| `CachedQueryResultsGenerateStatisticsTest` (the doc's repro class) | **PASS**, 12 s wall, `ms=10962` | doc records PASS, 14 s, `ms=11567` |
| `PostgreSQLContainer("postgres:18.4").start()`, isolated | **2 992 ms** | 4 247 ms |
| first 40 classes of `testlist.txt`, 4 shards | **PASS=39 NOTESTS=1** | — |

(The same 40-class slice re-run on the fixed binary is also `PASS=39
NOTESTS=1` with the same all-zero census, i.e. the fixes below change no
hibernate-reactive outcome — there was none left to change.)

The census the doc is *about*, over that whole 40-class run:

```
before-timeouts: 0
blocked-thread:  0
codebuf-bails:   0
```

CratonVM is marginally **faster** than the HotSpot figure the doc itself
recorded, both on the class and on the container start it blames. There is no
`BlockedThreadChecker` warning anywhere in the run, and no `before()` timeout.

Repro, for anyone re-checking:

```bash
export TESTCONTAINERS_RYUK_DISABLED=true
cd /data/cratonvm/apps/hibernate-suite-runner
echo org.hibernate.reactive.CachedQueryResultsGenerateStatisticsTest > /tmp/one.txt
CV_BIN=<binary> bash run-hibernate-suite.sh --list /tmp/one.txt \
  --shards 1 --timeout 420 --out /tmp/hr
```

### Why the numbers moved

Two blockers that were dominating hibernate-reactive on this host were
root-caused and fixed on 2026-08-12, hours after this doc was filed, and are
both on `dev`:

* `docs/internal/fixed-suite-bugs/jna-native-clinit-nativeversion-npe-20260812-FIXED.md`
  — JNI `FindClass` could not return `java.lang.Object` (a `jclass` **is** a
  `ClassId`, and `ClassId(0)` is `java.lang.Object`, so the one class every
  `JNI_OnLoad` asks for first was the one class `FindClass` reported as
  missing). This broke Testcontainers' rootless-Docker probe outright.
* `docs/internal/fixed-suite-bugs/vertx-pg-sasl-scram-handshake-fails-20260812-FIXED.md`
  — `javax/crypto/Mac.doFinal([BI)V` was unregistered, so SCRAM's PBKDF2 died
  on iteration 2 of 4096 and every DB-required class lost its session.

### What is NOT verified

**The original Windows/Docker-Desktop environment was not re-tested** — Docker
Desktop is not running on that machine now, so the doc's own run cannot be
repeated. Two properties of that run are worth recording before treating its
numbers as a CratonVM measurement at all: it had **16 concurrent CratonVM forks
plus 16 Postgres containers** live on a laptop, and the doc's own repro note
says exact wall-clock "varies with host load". A `BlockedThreadChecker` warning
at 3 982 ms against a 2 000 ms limit is what an oversubscribed host produces on
*any* JVM. The Linux census above cannot separate "the Windows VM was slow" from
"the Windows host was 16x oversubscribed", and neither can the original run.

---

## 2. The JIT bailout was not the cost

The doc asked directly: *"Worth checking whether the JIT bailout itself
(code-buffer-size misestimation forcing a bail-and-retry) is the primary cost,
or whether it's just a visible symptom."* Measured — it is neither the primary
cost nor even frequent.

A probe that pushes the whole Jackson bean-deserializer build path hot (a fresh
`ObjectMapper` per iteration, so nothing is cached and `resolve()` re-runs every
time; 1 500 iterations, ~20 s of CratonVM time) produces **exactly one** bail:

```
JIT compile bailed: code buffer estimate too small; retrying at the measured size
  method="com/fasterxml/jackson/databind/deser/BeanDeserializerFactory.addBeanProps:(...)V"
  code_len=847 capacity=171424 wanted=177275
```

That is a genuine 3.4% shortfall on a 167 KiB buffer, and it converges on the
retry — the message appears once and the method compiles. The method the doc
names, `BeanDeserializerBase._resolveManagedReferenceProperty`, does **not**
bail: `CRATONVM_DBG_CALLEE_PROBE=1` reports it `NEWLY-ADMITTED` (compiled) on
the same run.

Where the Jackson time actually goes, from `--stack-sample-ms 20` +
`--dump-native-registry`: 6.85 M native invocations for 1 500 iterations
(~4 566 per iteration, 84% of them `bridge` natives), with `AnnotatedClass.<init>`
the heaviest leaf at 15.5%. That is CratonVM's real-JDK bridge architecture on a
reflection-saturated workload, not a discrete defect — and it is why the
remaining gap to HotSpot on this probe is still ~30x after both fixes below.

---

## 3. Defect A — the code-buffer bail was mislabelled and could retry forever

Found by reading the channel the doc's log line comes out of. Two bugs, one
cause.

`ExecutableBuffer::mark_overflowed()` was the shared "discard this method"
channel for **ten** codegen sites, and only two of them are capacity problems:

| site | reason it discards the method |
|---|---|
| `emit` / `emit_byte` / `try_patch_*` bounds | genuine capacity overflow |
| `patch_branches` / `patch_self_calls` | patch offset past emitted length |
| `patch_rel8_or_bail` | `rel8` displacement out of `i8` range |
| `runtime_lowering::patch_rel32_to_here` | `rel32` displacement out of `i32` range |
| `modrm_rbp_disp`, `emit_movq_mem_rbp_from_xmm`, `emit_movq_xmm_from_mem_rbp` | frame displacement with no ModRM form |
| `emit_load_caller_arg` | caller-arg displacement with no form |
| `emit_mov_rsp_disp_from_reg` | RSP displacement with no form |
| `emit_test_mem8_imm8` | RSP/R12 base needs a SIB this form does not write |
| `simd.rs` array-compare unroll | disp8 required, value exceeds 127 |
| `deopt_stubs.rs` | deopt stub whose frame reserved no register-save area |
| `bytecode_walk.rs` | `invokedynamic` trap with an unresumable frame state |

Every one of the last nine was reported as **"code buffer estimate too small;
retrying at the measured size"**, and that cost two things:

**It named the wrong defect.** A displacement that has no encoding is not a
sizing problem. On such a bail `wanted` is typically well **under** `capacity`,
so the printed line contradicted itself, and no reader could tell which of ten
sites had fired.

**It retried forever.** The overflow bail is the one bail `try_compile` exempts
from the permanent bail list, on the theory that the next attempt sizes from a
measurement instead of the heuristic. But the hint was `wanted * 2`, and the
driver takes `estimated_size.max(hint)` — so whenever `wanted` was below half
the heuristic (which is exactly the non-capacity case) **the recomputed size was
identical**. The method was re-lowered in full, with a fresh `mmap`/`munmap` of
the estimate, on the calling thread, at every warmup-gate re-attempt (every
~2 000 invocations) for the life of the process — always failing in the same
place, never becoming compiled. That is what "repeated JIT bailouts on the exact
same Jackson method" describes, and it had no stopping condition at all.

### Fix

* `ExecutableBuffer::mark_codegen_unencodable(reason)` replaces
  `mark_overflowed()` at all nine non-capacity sites. It still sets
  `overflowed`, so every existing `if buf.overflowed() { return None }` discard
  keeps working byte-for-byte; it additionally records a `&'static str` reason.
* The driver reports named reasons on their own line —
  *"codegen invariant cannot be encoded; method stays interpreted"* with
  `reason=` — and returns without recording a shortfall, so `try_compile`
  bail-lists the method after **one** attempt like any other permanent refusal.
* The genuine-overflow path now folds the **capacity that failed** into the
  recorded measurement, so `code_buffer_hint` is always strictly greater than
  the size that just failed and each retry allocates strictly more than the
  last.
* `MAX_CODE_BUFFER_RETRIES = 3` (8x the first failing capacity) bounds the
  exemption. `code_buffer_retries_exhausted()` makes the refusal permanent after
  that, so even a still-genuine overflow cannot re-lower on every re-attempt
  forever.

Covered by `jit/src/lib.rs::code_buffer_retry_tests` — including
`the_hint_always_exceeds_the_capacity_that_failed` (the non-convergence, pinned
with the real 171 424-byte capacity from the Jackson bail above),
`the_retry_exemption_runs_out`, and
`an_unencodable_codegen_invariant_is_not_reported_as_a_short_buffer`.

---

## 4. Defect B — a lambda call cost 82x the identical interface call

This is the one that was actually worth throughput, and it is the one that fits
the doc's title: Vert.x, Hibernate Reactive and Jackson are lambda-saturated, so
a per-lambda-call tax lands squarely on the event loop.

`DispatchCostProbe2` runs six loops that differ **only** in the receiver, each
in its own small method so the JIT reaches it by invocation count. Steady-state
ns/call, before any fix:

| arm | CratonVM | HotSpot |
|---|---:|---:|
| bare loop | 0.7 | 0.2 |
| `invokestatic` | 4.1 | 0.2 |
| `invokevirtual` | 5.9 | 0.2 |
| `invokeinterface` on a normal class | 6.2 | 0.2 |
| **`invokeinterface` on a lambda** | **508.8** | 0.3 |
| **`invokeinterface` on a method ref** | **509.9** | 0.2 |

`armLambda` and `armIface` are byte-identical bytecode over the same interface;
the only difference is that one receiver is a lambda proxy. 6.2 ns versus
508.8 ns, and — unlike every other arm — it did not improve across six rounds,
so the JIT never closes it.

`perf record` on a lambda-only probe named the cause immediately:

```
10.62%  lambda::try_lambda_dispatch
 7.45%  <LambdaCallSite as Clone>::clone
 7.19%  mi_free
 6.77%  __memmove_avx512_unaligned_erms
 6.03%  drop_glue::<LambdaCallSite>
 4.44%  invoke::split_method_descriptor
```

~36% of the run was `malloc`/`free`/`memmove`, and it was all per-call
bookkeeping:

* **`lambda_proxies` stored `LambdaCallSite` by value.** `try_lambda_dispatch`
  has to take an owned copy out from under the `RwLock` before it can run the
  lambda body (the body can register proxies, so the guard cannot be held), so
  every lambda call **deep-cloned** the call site: six `Arc<str>` refcount pairs
  plus a fresh `Vec<char>`, then dropped them.
* **`split_method_descriptor` allocates a `String` per parameter**, and the
  lambda path called it four to six times per invocation — twice in
  `try_lambda_dispatch` purely to read a return-type *character*, twice in
  `coerce_lambda_args`, again in `checkcast_lambda_instantiated_args`.
* **`coerce_lambda_args` did all of that work for nothing** on the commonest
  lambda shape. For a non-capturing lambda whose SAM, implementation and
  instantiated descriptors agree (`x -> x + 1`, `Foo::bar`), every
  `coerce_arg(tok, tok, v)` returns `v` untouched and the `checkcast` replay
  skips every parameter — so the two descriptor walks, the `handles` vector and
  the per-argument pin push/truncate were pure overhead.

### Fix

* `lambda_proxies: RwLock<FxHashMap<ClassId, Arc<LambdaCallSite>>>` — the copy
  is now one refcount bump.
* `split_method_descriptor_ref` (borrowing; one `Vec<&str>` instead of N
  `String`s) and `descriptor_return_ref` (zero-allocation, for the two sites
  that only wanted the return token). `split_method_descriptor` now delegates to
  the borrowing form so the two cannot disagree about token boundaries.
* A provable no-op early return in `coerce_lambda_args` for the
  identical-descriptors, no-captures, no-receiver case. `coerce_arg`'s
  `if sam_tok == impl_tok { return Ok(v) }` is what makes it equivalent, and
  carries a comment saying so.

### Measured (A-B-B-A-A-B interleaved, same host, same load window)

Absolute ns/call on this host moves with load — 8 cores shared with every other
agent — so the ratio is the number, and the control is the four arms that must
NOT move.

| arm | before | after | ratio |
|---|---:|---:|---:|
| `invokeinterface` (lambda) | 813.8 | **462.3** | **1.76x** |
| `invokeinterface` (method ref) | 838.9 | **462.1** | **1.82x** |
| bare loop | 1.1 | 1.2 | 1.00x |
| `invokestatic` | 6.6 | 6.6 | 1.00x |
| `invokevirtual` | 9.9 | 10.3 | 1.00x |
| `invokeinterface` (class) | 10.6 | 10.1 | 1.00x |

Every `before` above every `after`, across three interleaved pairs, with the
four control arms flat.

### What this does NOT move: the Jackson probe

`JacksonResolveProbe` end-to-end is **unchanged** — mean 10 106 ms before,
9 987 ms after, **ratio 1.01** over 12 interleaved runs per arm. It is worth
recording how nearly this measurement went the other way: an earlier three-pair
run of the same comparison read 19 671 -> 16 598 ms, "1.19x", with every `after`
below every `before`. That was **host-load drift**, not the fix. Twelve pairs
made it vanish. Three consistent-looking pairs on a box eight other agents are
using is not a result.

The real reading is that the two probes measure different things, and only one
of them is lambda-bound. Jackson's bean introspection is
reflection-native-bound: 6.85 M native invocations per 1 500 iterations,
`AnnotatedClass.<init>` the heaviest leaf. Lambdas barely appear in it. The
1.76x is real and lands wherever lambdas are hot — Vert.x handlers, reactive
pipelines, stream chains — and the ~30x gap to HotSpot's 562-692 ms on the
Jackson probe is the bridge-native architecture in section 2, which this record
does not close and does not claim to.

---

## Gate state

`cargo test -p cratonvm-jit --lib`, `cargo test -p cratonvm-vm --lib` and
`cargo check --workspace --features synthetic-jdk` pass. Four gates are red, and
**every one was verified byte-identically red on pristine `origin/dev`
1c4ce7d3a** in a throwaway worktree before being attributed anywhere:

| red gate | pristine-dev check |
|---|---|
| `cargo clippy --workspace --all-targets -- -D warnings` | same errors, in `native-io`, `classloading`, `gc`, and `jit/src/x64/safepoint.rs` — no file this change touches |
| `cargo test -p cratonvm-cli --test jit_compile_gate_doors` | same panic, `jit_thread_mut: aliasing &mut JvmThread borrow detected` |
| `native-builtins logmanager::tests::t19_h3_*` (3 tests) | same 3 failures |
| `cratonvm-vm --lib --features synthetic-jdk :: vm::tests::object_output_stream_p70` | same assertion, `left: Int(0) right: Object(None)` |

The synthetic-jdk gate did surface one failure that WAS this change's: seven
`Arc<LambdaCallSite>` type errors in `vm/src/vm/tests.rs`, a module the default
`--all-targets` check never compiles. Fixed here; the same feature build now
runs 4 016 tests with only the pre-existing `p70` failure. Worth remembering
that `cargo check -p cratonvm-vm --all-targets` passing says nothing about the
feature-gated half of the tree.

## Residuals

* **Ryuk.** The doc's Windows note that Testcontainers' Ryuk reaper could not be
  reached from CratonVM is not reproducible here for the opposite reason: this
  host's runs use `TESTCONTAINERS_RYUK_DISABLED=true` throughout (the suite
  runner's convention), so the Ryuk path was never exercised. Still open as an
  untested path, not as a known defect. Whoever picks it up should run one class
  **without** that variable set.
* **The remaining lambda cost is ~450 ns against ~10 ns for the equivalent
  interface call.**

  **CORRECTION (2026-08-12, same day).** This bullet previously claimed the
  structural residual was that "the `MethodHandleKind::InvokeStatic` arm
  resolves its implementation through `invoke_shared` by class name, member name
  and descriptor on every call, where the `InvokeVirtual`/`InvokeInterface` arm
  has had `try_invoke_cached_lambda_impl`". **That is false.** The static arm has
  routed through `lambda_global_impl_owner` -> `try_invoke_cached_lambda_impl`
  since `a938584d9` (2026-08-10), which predates this record's own base commit.
  The claim came from reading a 50-line window of that arm that ended two lines
  before the `else if let Some(owner) = lambda_global_impl_owner(...)` branch and
  reporting the absence as fact — a truncated read taken as an absence proof, on
  a file this record had already been burned by once.

  **What the residual actually is**, measured rather than read, with the in-tree
  `CRATONVM_DBG_LAMBDA_PROF=1` instrument (10 runs per arm, A-B-B-A):

  | bucket | what it covers | ns/call |
  |---|---|---:|
  | `lookup` | `lambda_proxies` read + the (now `Arc`) call-site clone | 39 |
  | `prep` | SAM checks, capture prepend, descriptors, `coerce_lambda_args` | 56 |
  | `target` | the impl invoke, incl. whatever resolution its entry point does | 220 |
  | `other` | entry guards, `coerce_return`, **and the instrument's own ~36 ns** | 147 |

  The **loader-faithful override lookups were the removable part of `target`**
  and are now removed: `lambda_impl_dispatch_override` and its `_driven` sibling
  took a `lambda_proxy_hosts` read lock (twice) plus a `class_manager` read lock
  per lambda call to re-derive a per-proxy constant that is `None` for every
  program without a custom classloader. Both now short-circuit on
  `any_defining_loader_registered()` — a lock-free atomic whose `false`
  guarantees no `ClassId` anywhere has a `UserDefined` loader id, which is the
  same invariant `lookup_loader_initiated` (the function
  `lambda_impl_dispatch_override` delegates to) has relied on for every
  `new`/`checkcast`/`instanceof` in the VM. Behaviour is therefore unchanged by
  construction for the passive half, and for the `_driven` half the atomic
  subsumes its own `UserDefined(_)` test four lines further down.

  Measured: **`target` 249.8 -> 219.8 ns (1.136x), ranges non-overlapping**
  (base 239-258, fix 210-231), with `lookup`, `prep` and `other` flat to within
  0.1 ns — the change moves exactly the bucket it should and nothing else.
  Whole-dispatch **494.0 -> 464.4 ns (1.064x)** under the profiler, **421.5 ->
  385.0 ns (1.09x)** by wall clock with the four control arms flat.

  Negative control: the three hibernate-reactive `@BytecodeEnhanced`
  integration classes — the exact workload
  `lambda_impl_dispatch_override_driven`'s doc cites, a lambda in a
  bytecode-enhancing loader's copy of a class whose impl owner also exists
  globally — pass identically before and after (`ReferenceBETest` 7/7,
  `LazyBasicFieldTest` 4/4, `LazyOneToOneBETest` 3/3).

  **Still open after that**: `target` is 220 ns for a body that adds 1, which is
  cached-frame build + interpret + teardown (`Frame::new_pooled_cached`,
  `init_locals_from_parts`, `pop_and_recycle_frame_with_reason` and the two
  `drop_glue`s were ~16% of a `perf` profile) — shared interpreter cost, not
  lambda-specific. `other` is ~110 ns net of the instrument and is the entry
  guards plus `coerce_return`. Neither is a defect; both are the interpreter
  architecture.
* **The 71-class investigation index** (`investigate-INDEX.md` and its six
  batch pages) was built from the partial Azure run that the SASL/SCRAM and JNA
  bugs dominated. With both of those fixed and 39/40 of the head of
  `testlist.txt` now passing, that list is stale as a defect list and should be
  regenerated from a full run before anyone works it class by class.
