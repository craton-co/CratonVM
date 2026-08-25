# The interpreted invoke costs ~350 ns, and the biggest piece is moving a `Frame`

| | |
|---|---|
| **Status** | OPEN — a tracked throughput item with a measured target, not a failing test |
| **Severity** | low — no test fails on this; it is the ceiling several suites sit under |
| **HotSpot** | interpreter (`-Xint`) pays ~4 ns for the same operation |
| **CratonVM** | ~350 ns per interpreted `invokevirtual` (Windows and Linux, independently) |
| **Opened** | 2026-08-25, inheriting criterion 3 of the retired `webapp-deploy-annotation-scan-interpreted-226x` write-up |

This page is the successor to the Tomcat annotation-scan investigation, which
ran from 2026-08-03 to 2026-08-24 and retired with its own two exit criteria
met. Its third criterion was explicitly *not* its own work:

> **The gap closes with the direct-compiled-call project, not here.**

That is this page. Everything below is carried over from four sessions of
measurement on that page so the numbers are not re-derived; the Tomcat-specific
material stayed with the retired write-up.

## What the number is

`probes/InvokeAttributionProbe.java` puts a three-bytecode `callee()` behind an
`invokevirtual` in a loop, so nearly all of the loop's cost is invoke overhead
**by construction**, and prints the per-iteration delta against the same loop
with the call written out. It is an 8-second A/B harness — use it to price a
candidate change rather than paying for a 35-second scan.

| | Windows | Linux (Azure) |
|---|---:|---:|
| `withCall` | 388–512 ns/iteration | 450–455 |
| `noCall` (control) | 94–124 | 94–101 |
| **invoke delta** | **290–417 ns** | **353–370** |

`probes/CallFloorProbe.java` under `HotSpot -Xint` against `CratonVM --nojit`
prices the two interpreters directly (ns/op), and the *increments* are the
number that matters:

| body | HotSpot `-Xint` | CratonVM `--nojit` | ratio |
|---|---:|---:|---:|
| arith (no call) | 17.2 | 176.1 | 10x |
| + invokestatic leaf | 21.1 | 435.7 | **21x** |
| + invokevirtual leaf | 17.2 | 666.0 | **39x** |
| + invokeinterface leaf | 17.0 | 679.7 | **40x** |
| + `String.length()` | 26.6 | 1105.1 | **42x** |

Adding one call costs HotSpot's interpreter ~4 ns and CratonVM's **~260 ns
(static) to ~490 ns (virtual)** — **65–120x**. Straight-line bytecode is only
10x. This VM's interpreter is respectable at arithmetic and catastrophic at
**invoke**.

## Where the 350 ns goes

Azure, `--nojit`, `perf record -F 997`, flat. The point of the probe's two arms
is that the `nocall` arm is a **control**, and it is a remarkably clean one —
three symbols and nothing else (`execute_frame_from_index` 77.40%,
`safepoint_check` 18.17%, `try_osr_with_backoff` 3.07%). **So every other symbol
in the `call` arm is the invoke path**, which makes the following a
decomposition rather than a list:

| group | share of the `call` arm |
|---|---:|
| **frame lifecycle** (construct, fill locals, move in, move out, drop) | **~24.7%** |
| dispatcher body (`execute_invokevirtual_cached` + its closure) | ~17.1% |
| receiver / heap checks | ~6.6% |
| inline-cache lookup + `CachedInvokeTarget::clone` | ~5.0% |
| class-manager `RwLock` read, per invoke | ~3.7% |
| native-interception chain | ~3.0% |
| argument decode | ~2.3% |

**The largest single item is not the dispatcher, it is the frame.** A
`--call-graph` run puts the `memcpy` under `pop_and_recycle_frame_with_reason`
→ `Vec::pop<Frame>` and under `push_frame_and_fire_entry`: `Frame` is a large
by-value struct and it is **moved on every push and every pop**. That is a
data-structure change to the interpreter's frame stack, and it is the shape of
the remaining gap.

## What has already been taken off it

Four pieces were removed by the retired page's sessions. They are recorded here
because each one is *gone* — a future profile will not show them, and a reader
comparing against the shares above needs to know why.

1. **The returning frame is recycled in place** (2026-08-11). The return path
   moved a ~300-byte `Frame` three times to arrive at four `Vec` headers; it now
   reads the dying frame through a borrow and harvests the buffers by header.
   **−5.3 percentage points of the invoke arm**, entirely in the two symbols the
   change targets, every other symbol flat.
2. **The interception chain is classified once per call site** (2026-08-11).
   `CachedBytecodeMethod` carries an `intercept_shape_cache: OnceLock<u8>`.
   `intercept_force_registered_native_cached` 1.44% → 0.99% and
   `real_http_url_connection_native` (1.29%) left the hot path entirely — **a
   64% cut** of that group.
3. **Half the per-invoke class-manager lock** (2026-08-11). The virtual tier-up
   gate computed two predicates into eager `let` bindings above the `if` that
   consumes them, so a short-circuiting `&&` chain never got to short-circuit.
   Both are now closures called in place. `try_read` disappeared from the
   profile, and on a quiet host (load 3.5) six interleaved passes gave
   **−3.8%, 6/6, with no overlap between the columns**.
4. **The two receiver-shape gates** (2026-08-24). `execute_invokevirtual_cached`
   asked, on every non-`invokespecial` virtual invoke, whether the receiver
   class was a lambda proxy (`lambda_proxies.read().contains_key`) and whether
   it was the synthetic `AnnotationProxy` (class-manager `read()` +
   `get_class` + an `Arc<str>` comparison against a literal). Both are
   per-cache-entry constants. They are now `ClassRealm::is_lambda_proxy_class`
   (a range test on the reserved proxy id space, so the map is probed only for
   ids that could be in it) and `ClassRealm::is_annotation_proxy_class` (a
   memoized `ClassId` identity test whose negative half is keyed on
   `class_definition_epoch`). See the honest note on this one below.

**A recorded negative, because it is the interesting half.** The first version
of piece 2 added a `shape == 0` early return into a shared tail function, on the
reasoning that the common call site should not even step over three bit tests.
Measured, that was **worse than leaving the control flow alone**: entry 1.05% +
tail 1.12% = 2.17%, against 0.99% for the same string-work removal with the arms
guarded in place. The function boundary cost more than the three bit tests it
skipped.

### Piece 4 did not separate on the wall clock, and the host is why

Recorded plainly so it is not quoted as a win it did not demonstrate.
`InvokeAttributionProbe`, ten interleaved passes with the arm order reversed on
even passes, on a box at load 33–43:

| arm | withCall min | withCall median | noCall min | noCall median |
|---|---:|---:|---:|---:|
| before | 701 | 935 | 190 | 243 |
| after | 774 | 887 | 158 | 196 |

The `noCall` arm contains **no invoke at all** and cannot be affected by the
change — and it moved as much as the test arm did, in the same direction. That
is the definition of an unresolved measurement. `perf` shares did not separate
it either: `execute_invokevirtual_cached` read 14.71% before and 14.75% after,
against 0.7 pp of run-to-run drift on symbols the change does not touch.

The change was kept anyway, and the reason is worth stating because this
investigation reverted other levers that "measured nothing": those **added** a
cache and its invalidation cost, and had to earn their keep. This one **deletes**
two lock acquisitions and a string comparison from a correctness gate on the
hottest path, and is net −40 lines. It is not evidence of a speed-up and is not
recorded as one. **Any future claim about it needs a quiet host** — see
§ Measuring this at all.

## Untaken levers

Both are structurally real, and neither is a point fix.

* **The direct compiled call is illegal for handler-bearing and `java.util`
  callees.** `CRATONVM_DBG=jit-method-stats` sees 271 methods and 133 534
  invocations; `CRATONVM_DBG=invokestats` sees **22 400 001 inline-cache hits**
  in the same run. **99.4% of invokes never reach the tier-up counter**, and
  only 3 methods are `hot_but_stuck_in_interpreter`, so the manager does not
  even know it is blind. The exclusions live in `execute_invokevirtual_cached`'s
  tier-up block: `!is_special`, `!cached.is_synchronized`,
  `!has_registered_native`, `!receiver_is_java_util`,
  `cached.exception_table.is_empty()`.

  **Do not "just count them".** That was built and measured
  (`CRATONVM_JIT=special-tierup`, reverted): counted invocations moved
  129 391 → 129 455, the compiled census 672 → 674, and wall-clock got *worse*.
  Compiling a method whose only consumer is the very call site that excluded it
  buys nothing. The lever is making the **direct compiled call legal** — i.e.
  exception resumption across a direct compiled call, and the stale
  receiver-specific entry that `receiver_is_java_util` was added to avoid.

* **`invokespecial` call sites stop feeding the tiered manager once cached.**
  `dispatch_virtual.rs`'s invocation-counter block opens with `if !is_special`,
  and it gates *counting* as well as promotion. The uncached route in
  `interpreter.rs` still counts every method regardless of opcode, which is why
  constructors are found compiled anyway — so the obvious framing
  ("constructors are invisible to tier-up") is **not** what the code does.
  Splitting counting from promotion is the small change; the blast radius is
  what makes it a project, because widening which methods reach the optimizing
  tier moves work off the single-pass backend, whose loop lowerings have no
  IR-tier equivalent.

* **`new` has no per-call-site class-resolution cache.** `opcodes.rs`'s
  `Instruction::New` re-resolves its constant-pool entry on every execution,
  unlike `put_field`/`put_method`. A constant-pool parse pays this once per
  entry.

Whichever is picked up needs `regression-suite/perf/c2-reach.sh` plus a
CratonBench pass in scope: all three move work between the tiers those gates
watch.

## What is NOT worth re-deriving

Each of these was measured and falsified on the retired page. They are listed so
the next reader does not spend a session on them again.

* **Not the `ACC_SYNCHRONIZED` JIT-admission gate.** Admitting them
  (`CRATONVM_JIT=sync-methods`) changed the annotation scan by nothing:
  2237/2064 → 2140/2096 µs/class.
* **Not the outer loops failing to tier up.** `CRATONVM_JIT=loop-work-tierup`
  compiles `ConstantPool.<init>` and buys 2–3%, inside the noise.
* **Not by-name field resolution.** A thread-local, epoch-validated
  `(ClassId, name) -> slot` memo measured **1.9% over ten interleaved runs per
  arm** — inside the noise. Reverted, not shipped.
* **Not the native-call funnel.** Priced end to end with the two in-tree
  profilers at ≈120 ns for a compiled-code native call: 12.69 M calls × 120 ns
  ≈ 1.5 s of a 15.5 s webapp deploy — **10%**. Driving it to zero leaves ~15x
  against HotSpot, not 5x.
* **Not `try_stackless_invoke`.** `CRATONVM_DBG=invokestats` over a scan:
  `cache_hit=600001 cache_miss=1784 vtable_fast=2672 slow_path=3970`. The
  monomorphic inline cache is **99.7% warm**, and `try_stackless_invoke` is the
  cache-*miss* path. Its comparison count is irrelevant no matter how large.
* **Not the reflective type checks.** `Class.isAssignableFrom` / `Class.cast`
  answer from class ids rather than names since 2026-08-07 (1.40x / 1.46x on a
  microbenchmark) and it moved the real workload by nothing: 1.77 M pairs ×
  114 ns ≈ 0.2 s of 122 s.

A related measurement worth keeping: `probes/NativeBridgeCostProbe.java` prices
a registered native against a byte-for-byte equivalent body that has no
registration, in the same process. `Objects.requireNonNull` 577 ns bridged vs
240 ns as bytecode; `Class.isAssignableFrom`+`cast` 1478 ns vs 12 ns. On HotSpot
both columns are 1–5 ns and the ratio is ~1. **A bridge over a five-bytecode
body is a pessimization here**, and one webapp deploy makes 1.73 M
`requireNonNull` calls. That is a registry-shaped question rather than an
interpreter one, which is why it is recorded and not acted on here.

## Measuring this at all

The Azure build host is shared, and during these investigations its load ran
between 1.4 and 178. The **same binary and configuration** measured 2237 and
14789 µs/class an hour apart, and HotSpot's own column swung 13.5 → 99.0
µs/class across three interleaved rounds — and 20.9 → 157.0 on 2026-08-24.

* Any number taken at load > 10 is noise. Check `/proc/loadavg` first and print
  it **with** the result.
* Interleave the arms in both directions, and prefer CratonVM-vs-CratonVM A/B
  over the cross-VM ratio.
* Prefer `perf` shares to wall clock — they are load-independent — but note that
  they resolve about 1 pp at best, so a ~2 pp change needs a quiet host too.
* **Use a frame-pointer build for any call-graph question on this binary.**
  `--call-graph=dwarf` named two inlined callers that provably take no lock, and
  `--no-inline` collapsed the chains to a bare address. A rebuild with
  `RUSTFLAGS="-C force-frame-pointers=yes"` and `perf record --call-graph=fp`
  named the caller immediately and correctly.
* **An inline attribution has to be checked against the source before it is
  believed.** `perf` attributed a 3.16% `memcpy` arm to `dbg_loader_trace`
  inlined inside `execute_invokevirtual_cached`, which would have been a
  spectacular find — a debug predicate copying memory on every invoke. It is not
  real: `dbg_loader_trace()` is `cached_is_ok!`, a memoised `MemoSlot` load that
  cannot copy anything.
* **`pc=0 last_pc=0` in a `--stack-sample-ms` profile is the INVOKE, not the
  callee.** The sampling hook sits at the top of the dispatch loop, and an
  invoke pushes the callee frame and `continue`s — so the first iteration that
  can observe a re-armed sample request sees the CALLEE, at `pc=0 last_pc=0`,
  having executed nothing. Aggregating leaf frames by method files the invoke's
  cost under the callee's name, where it reads as "this body is slow". Three
  profiles on the retired page were read wrong this way. Calibrated:
  `InvokeAttributionProbe` puts 37 of 69 samples (53.6%) at `callee`
  `pc=0 last_pc=0` and **one** anywhere in `callee`'s body.

## Exit criteria

There is deliberately no throughput number here. Setting one before the
direct-compiled-call project scopes itself would be inventing it — that is the
mistake the retired page made and had to re-scope out of.

What this page asks for instead:

1. **The frame push/pop moves are gone.** `Frame::new_pooled_cached`,
   `init_locals_from_parts` and `copy_args_to_locals` are the push side.
   The symmetric fix to piece 1 is **not** justified on the current evidence:
   `push_frame_and_fire_entry` no longer appears among the `memcpy` callers at
   all after that change, so the push-side move is either already elided by the
   compiler or below 0.5%. **Re-measure before building it.**
2. **The direct compiled call is legal for handler-bearing and `java.util`
   callees**, with `regression-suite/perf/c2-reach.sh` and a CratonBench pass
   green.
3. **`InvokeAttributionProbe`'s invoke delta is reported on a host at load < 10**,
   in both arm orders, with the `noCall` control printed beside it. No claim on
   this page is accepted without its control column.

## Reproduction

```bash
javac -d /tmp/probeout probes/InvokeAttributionProbe.java
<cratonvm> --java-home <real JDK 25> --Xmx 1g --nojit -c /tmp/probeout InvokeAttributionProbe 8000000 both 3
```

`both` self-times and prints `withCall_ns`, `noCall_ns` and `invokeDelta_ns` per
round. `call` and `nocall` select a single arm, which is what a native profiler
needs: recording both in one process mixes them and no symbol can be attributed.
