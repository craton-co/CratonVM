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

## The 2026-09-02 pass: five costs that were fixed per method and paid per operation

A separate probe set (below) priced the interpreter against HotSpot's template
interpreter operation by operation, rather than pricing one workload. The shape
that came out is worth stating before the individual items, because it is what
made them findable:

| operation | CratonVM `--nojit` | HotSpot `-Xint` | ratio |
|---|---:|---:|---:|
| one bytecode, straight-line arithmetic | 11.0 ns | 0.86 ns | 12.8x |
| one backward branch | 33-37 ns | 4.9 ns | 7.1x |
| instance field read or write | 160-200 ns | ~1 ns | ~170x |
| static field read or write | 63-68 ns | ~1 ns | ~65x |
| `invokestatic`, 0 args, fixed | 268-289 ns | 14.4 ns | 19x |
| each extra `int` argument | ~35 ns | ~0.85 ns | 41x |
| `invokevirtual` over `invokestatic` | 151-213 ns | -0.2 ns | - |
| **`invokeinterface` over `invokevirtual`** | **114 ns** | **3.4 ns** | 33x |
| `tableswitch` loop iteration | 209 ns | 18.2 ns | 11.5x |
| `byte[]` load + store iteration | 322 ns | 23.3 ns | 13.8x |

Straight-line bytecode, switches and array access all sit in one 9-14x band.
Everything that crosses a **method boundary** or touches an **object field** is
one to two orders worse - and in each case the excess turned out to be work
that is constant per method, or per call site, being redone per operation.

### What was fixed

Every figure below is an **internal control**: a difference between two arms in
the same process, measured on ONE binary with the change's own kill switch, arms
interleaved in both orders. The development host ran at 73-94% CPU throughout,
so no absolute number here is a quiet-host number - but a difference between two
arms of one process is not a wall-clock claim.

1. **`retarget_instance_field_to_receiver` ran on every instance field access.**
   `CRATONVM_DBG_HOTPATH_COUNTS=1` reported `retarget_field=3000000` on a run
   with exactly three million field accesses. Its slow half exists only for the
   loader-split case - one class NAME under two `ClassId`s - and its first act
   is to discover the names differ and return `None`. Reaching that cost a
   `class_manager` read lock, three class lookups and a constant-pool walk, on
   the majority of accesses (any *inherited* field has receiver class != the
   declaring class). `any_duplicate_class_name()` is a one-way latch maintained
   off the `name_definitions` index the class manager already keeps, so the
   answer is one relaxed load.

   `probes/FieldShape.java`, inherited-minus-own field pair, ns:

   | | pass 1 | pass 2 | pass 3 | pass 4 |
   |---|---:|---:|---:|---:|
   | gate on | -5.6 | -28.6 | -14.5 | -37.7 |
   | gate off | +52.1 | +123.5 | +54.8 | +63.5 |

   4/4, no overlap. With the gate the delta collapses to ~0, which is what it
   must do once both arms run one path. Kill switch:
   `CRATONVM_LOADER_NO_DUP_NAME_FIELD_GATE=1`.

2. **Every backward branch called `safepoint_check` unconditionally**, then
   `continue`d into the loop-top poll thirty lines later, which asks
   `stw_requested` first and makes the same call. The redundant call's only
   unique work was the async-exception drain - a thread-local `RefCell` borrow,
   an `Arc::clone`, a `swap(AcqRel)` and an `Arc` drop, i.e. **three locked
   read-modify-writes per loop iteration**, inside a function too large to
   inline. The slot handle is now hoisted once per `execute_frame` (the same
   discipline as `pgo_enabled`) and the four back-edge sites test two relaxed
   loads.

   `probes/BackEdge.java` - two loops with identical total body-bytecode counts
   and an 8x difference in back-edge count, so the per-back-edge cost falls out
   of the difference. ns per back edge:

   | | pass 1 | pass 2 | pass 3 | pass 4 |
   |---|---:|---:|---:|---:|
   | gate on | 22.7 | 28.7 | 24.3 | 9.1 |
   | gate off | 30.2 | 34.2 | 41.6 | 25.5 |

   4/4 in the same direction; medians 23.5 vs 32.2. Kill switch:
   `CRATONVM_JIT_NO_BACKEDGE_POLL_GATE=1`.

3. **The interface receiver-selection re-check ran a hierarchy walk under a lock
   on every `invokeinterface` cache hit.** `invokeinterface` and
   `invokevirtual` reach the same dispatcher and differ in exactly that block,
   which is why the 114 ns above is attributable rather than inferred. It is now
   a receiver-equals-declaring short-circuit followed by `IfaceSelectSiteCache`,
   the existing audited `SiteCache` keyed on the call site and storing the
   `(receiver, declaring)` pair a walk verified. A polymorphic site misses and
   re-walks, exactly as before. Kill switch:
   `CRATONVM_JIT_NO_IFACE_SELECT_MEMO=1`; `CRATONVM_DBG_FIELD_SITE=1` adds
   `iface-select: hit / miss / fill / trivial`.

   Engagement first, because the wall clock could not have shown it. One
   900k-iteration probe arm of each shape:

   ```
   iface-select: hit=900003 miss=5 fill=5 trivial=900045
   ```

   `A implements I { f() }` takes the short-circuit; `B extends A` takes the
   memo, at a 99.9999% hit rate. Neither re-walks after the first call.

   `probes/Dispatch.java`, `ifaceInherited` minus `virtual1` — the arm whose
   receiver does NOT declare the method, so the short-circuit cannot fire and
   the memo has to answer. ns:

   | | pass 1 | pass 2 | pass 3 | pass 4 |
   |---|---:|---:|---:|---:|
   | memo on | -41.3 | -19.2 | +59.5 | -38.5 |
   | memo off | +88.5 | +88.8 | +60.5 | +44.6 |

   3/4 with no overlap; pass 3 is a tie under a load spike. Medians -29 vs
   +74, so roughly **100 ns** off an inherited-receiver interface call. With
   the memo on the delta goes NEGATIVE, which is the right shape rather than a
   suspicious one: both opcodes now reach the dispatcher through the same
   work, so what is left is noise around zero.

   **The first A/B of this change separated nothing, and the switch was why.**
   It gated the memo but not the short-circuit beside it, so its "off" arm was
   not the pre-change path — and the arm being measured (`iface1`, whose
   receiver declares the method) short-circuited in BOTH arms. A kill switch
   that does not restore the old path is not a control, and a probe arm the
   switch cannot reach is not a measurement. Widened to disable both steps;
   the numbers above are from the corrected switch, and `Dispatch.java` now
   prints the `ifaceInherited` delta so the right arm is the obvious one to
   read.

4. **Two descriptor scans that are constant per method.** `ParamTags::of`
   rescanned `cached.method_descriptor` on every invoke through the inline
   cache; `areturn` called `jit::return_type(frame.method_descriptor())` - a
   linear scan for the closing paren - on every reference return. Both now read
   a `DescriptorFacts` memoized on `CachedBytecodeMethod` beside the four memo
   cells already there.

   `probes/RetTag.java` - two arms with identical bodies and argument lists
   differing only in RETURN DESCRIPTOR LENGTH, so the delta is the scan. ns:

   | | pass 1 | pass 2 | pass 3 | pass 4 |
   |---|---:|---:|---:|---:|
   | memo on | -2.0 | +4.3 | +13.2 | -32.0 |
   | memo off | +69.0 | +35.2 | +36.1 | +20.9 |

   4/4, no overlap, and the memo-on delta collapses to ~0 because descriptor
   length stops mattering.

5. **Frame push wrote every argument slot twice.** `init_locals_from_parts`
   sized locals and kinds to `max_locals` with filler, then overwrote the
   leading argument slots. It now pushes the arguments and resizes the
   remainder, so each slot is written once. Not separately measured; it is a
   deletion, pinned byte-for-byte against the old layout by two tests covering
   category-2 arguments, unset tails, the defensive clamp, and pool recycling.

### A recorded non-separation, and a corrected claim

**The `ParamTags` half of item 4 did not separate.** `probes/Arity.java`
per-extra-argument slope, four passes per arm: memo on 41.9 / 54.1 / 45.3 /
49.2, memo off 39.1 / 53.6 / 24.8 / 44.4. The no-call control arm moved 10%
between passes and the effect is ~1% of the arm. It is recorded as unresolved,
not as a win. The change was kept on the same grounds this page already applies
to piece 4 above: it *deletes* a per-call string scan and adds no cache
invalidation, and its equivalence to both scans it replaces is pinned by
`param_tags_match_nth_param_tag_byte`.

**One claim in this pass was overstated and is corrected here.**
`ClassRealm::is_annotation_proxy_class` was read as taking a `class_manager`
read lock on *every* virtual invoke while classes were loading. It does not:
the cold resolver stamps `class_definition_epoch()` on its negative, so the
lock is taken once per class **definition**, not once per invoke. What is left
is an out-of-line `#[cold]` call plus two atomic loads on every virtual invoke,
for a class most programs never mint - real, but small.
`any_annotation_proxy_defined()` removes it in one relaxed load and is kept on
those grounds, not on a measurement. `CRATONVM_LOADER_NO_ANN_PROXY_LATCH=1`.

### The argument round-trip, which is still open

Each additional `int` argument costs ~35 ns against HotSpot's ~0.85 ns, and
about 11 ns of that is the caller's own `iload` bytecode, so ~25 ns per
argument is the invoke path itself. The shape is known and is **not** fixed:

The caller's operand stack already holds arguments as 8-byte `CompactValue`
slots and the callee's locals want exactly that representation, but between
them every argument goes `CompactValue -> Value -> CompactValue`:
`pop_arg_for_descriptor_checked` decodes into the 16-byte `Value` enum,
`args_buf: [Value; 16]` holds it (a 256-byte stack array initialised on every
invoke, zero-argument calls included), and `copy_args_to_locals` converts it
back via `from_value_kinded` plus `lkind_of_value`.

**Why it was not taken in this pass.** `args_slice: &[Value]` is consumed
between those two ends by the whole interception chain
(`intercept_classloader_set_default_assertion_status`,
`surefire_lazy_launcher_discover_native`,
`native_override_for_cached_reflect_invoke`,
`intercept_force_registered_native_cached`,
`try_execute_cached_trivial_instance_getter`), by the synchronized-method
monitor enter, and by JVMTI. Skipping the materialisation means restructuring
all of that, and `decode_by_descriptor` is exactly the code whose comments
record three separate already-fixed silent-corruption bugs (the BouncyCastle
SM2 `SUB_INT` collision, the JNI smuggled-`jobject` path, and `-0.0` under a
`D` descriptor). It is a project, not a point fix.

**What the next attempt should do.** Precompute the descriptor-driven
argument slot map alongside `DescriptorFacts` (the category-2 expansion is
static per method), then add a raw `CompactValue`-to-`CompactValue` transfer
for the case where the popped kind byte and compact tag already agree with the
descriptor tag, falling back to today's `Value` path for every shape that does
not. Keep it behind a switch and A/B it with `probes/Arity.java`'s
per-argument slope, whose control arm is the zero-argument call.

**Do not bother with the `[Value; 16]` initialisation on its own.** It is a
256-byte memset per invoke, which is ~1-2% of a 289 ns call - below this
host's resolution, and not worth the `MaybeUninit` it would take to remove.

### The aggregate does NOT resolvably move an end-to-end workload

Read this before quoting any per-operation number above as a speed-up.

Every figure in this pass is a **per-operation** delta taken with an internal
control. None of them is an end-to-end claim, and the end-to-end measurement
was taken separately and came back **unresolved**.

`bench/CratonBench.java` is the wrong instrument for it: at ~104 s per run it
is seven compute kernels that all tier up, so after warmup the interpreter is
barely on the path. The interpreted phase of a *shipped* configuration is
class loading and the reflective introspection a framework does before
anything tiers up — code that runs once or a handful of times and therefore
never reaches the JIT at all. That is the population these changes serve, and
`probes/ClassLoadShape.java` is the probe for it: 40 rounds over 40 JDK
classes, walking each one's declared methods, fields, constructors,
interfaces and superclass.

Its standing ratio, default configuration (JIT on), is itself worth recording:

| | CratonVM | HotSpot | ratio |
|---|---:|---:|---:|
| class load + reflect, 40x40 | 1,284 ms | 100 ms | **12.8x** |

The A/B, all five kill switches on versus off, arms interleaved in both
directions, two batches because the host's condition drifted between them:

| batch | passes | NEW median | OLD median | delta |
|---|---:|---:|---:|---:|
| 1 | 6 | 1,103 ms | 1,134 ms | 2.7% |
| 2 | 8 | 1,065 ms | 1,073 ms | 0.7% |

Sorted, batch 2 (the quieter one):

```
NEW  1002 1033 1060 1064 1067 1072 1084 1086
OLD  1014 1034 1046 1072 1074 1111 1112 1155
```

**What that supports, and what it does not.** The direction is consistently
favourable — NEW's median is lower in both batches, its minimum is lower, and
OLD carries the longer tail — but the magnitude is 0.7-2.7% against
distributions that overlap across most of their range. That is at or below
what this harness resolves on this host. It is **not** a 3% speed-up claim.
The first pass of batch 1 also read 1,858 ms for NEW against 1,262 for OLD,
which is a cold page cache and not a regression; it is excluded above and
named here so nobody rediscovers it as one.

**Why the per-operation wins do not add up to a visible end-to-end one.**
This workload's time is dominated by class *parsing*, verification and native
reflection, not by the specific operations that were made cheaper. A change
worth 30 ns on an inherited field access is worth 30 ns times however many
inherited field accesses the workload performs, and here that is a small
share of a second spent mostly elsewhere. The right reading is that the
interpreter's per-operation floor came down and the end-to-end ceiling is set
by something else — which is a statement about where to look next, not a
reason to revert anything.

**The changes are kept on that basis.** Each one deletes work that was being
repeated per operation and is fixed per method, per call site or per process;
each is pinned by an equivalence test; none adds a cache that needs
invalidating beyond a `OnceLock` on an immutable field or a one-way latch.
That is the same standard this page already applies to piece 4 of the
2026-08-11 work. A future end-to-end claim needs a quiet host and a workload
whose time actually sits in these operations — the Tomcat annotation scan the
retired predecessor page was built around is the obvious candidate, and it was
not run here.

## Five suite vectors were already red on dev when this pass landed

Recorded here, on the page whose branch was in flight at the time, so the next
reader does not spend a session attributing them to it. **None of them is
caused by the 2026-09-02 interpreter work**, and the control that proves it is
a build, not an argument.

| vector | signature |
|---|---|
| `RJitMultiArrayClass` | `s20-serial-roundtrip` COLD and HOT: `want=[[D] got=[threw-java.lang.ArrayIndexOutOfBoundsException]` |
| `RMapGcStress` | `rc=1: no output` |
| `RJdkIntrinsics3` | `NoSuchMethodError java/lang/StringBuilder.close()V`, then `ServiceConfigurationError: Locale provider adapter "CLDR" cannot be instantiated` |
| `REncodingFidelity` | output differs from HotSpot |
| `RBufferPoolCount` | `routeA.pools=[mapped, direct, mapped …]` |

### The control

`4be7404d6` is the dev tip immediately **before** the interpreter branch
merged; `git merge-base --is-ancestor` confirms none of the branch's commits
are in it. Built as `cratonvm-devctl-4be7404d6.exe` and run against the same
compiled vectors: **all five fail, with identical signatures.**

Three weaker checks agreed beforehand and are kept because each rules out a
different thing:

* All five still fail on the merged binary with **all five kill switches set**
  — so no switched change causes them.
* Three of the five (`RJitMultiArrayClass`, `RMapGcStress`, `RJdkIntrinsics3`)
  **pass** on a binary carrying the branch's changes against the older dev,
  including `push_args_to_locals`, the one change with **no** kill switch. That
  is the only way to exonerate an unswitched change, and it is why the binary
  was kept.
* The symptoms sit in unrelated subsystems: a locale provider, a charset
  fidelity diff, an NIO buffer-pool census. None of them touches interpreter
  dispatch, frame locals or the invoke path.

### What `RJitMultiArrayClass` actually is

Worth writing down because it is the one with a clean handle on it.

`--nojit` makes it **pass**, on pure dev and on the merged binary alike, so it
is a JIT defect. It is also not a property of the serialization scenario: s20
run **alone** passes, and 700 iterations of the same round-trip in isolation
pass. Any *single* preceding scenario — 3000 iterations of multi-dimensional
array work — is enough to make s20's very first (cold) call throw. So the
trigger is compilation of the array shapes, not anything serialization does.

`probes/` has no vector for this; the reproduction is the suite's own, with a
scenario filter:

```bash
# fails
cratonvm --java-home <JDK 25> -c regression-suite/build RJitMultiArrayClass
# passes — same binary, same class
cratonvm --java-home <JDK 25> --nojit -c regression-suite/build RJitMultiArrayClass
```

dev merged `perf/jit-six-findings-20260902` (a GP register file for the
optimizing tier, and reference stores that stop paying a call) inside the same
window. That is the obvious first place to look; it is a lead, not a finding —
no bisect was run.

### What this pass did NOT find, so nobody re-derives it

* **Fast-path arms for `tableswitch` / `lookupswitch` are not a lever.** Both do
  fall through to the decoded handler - confirmed, `decoded_instr` rises by
  exactly one per iteration - but they measure 11.0-11.5x, the same band as
  arithmetic that never leaves the fast path. The quickened stream plus
  `execute_instruction` costs about what a fast-path arm costs.
  `invokedynamic`, `newarray`, `anewarray`, `athrow`, `wide` and `goto_w` share
  that path; only `newarray` was priced (8.9x, dominated by allocation).
* **Field *resolution* caching is already solved.** `FieldSiteCache` measured
  `hit=5402347 miss=332` - 99.994%. The remaining field cost is downstream of
  resolution, which is how item 1 was found.
* **Array element access needs nothing of its own.** The whole `0x2e..=0x35`
  and store family is on the fast path under a range arm; the `byte[]` loop's
  13.8x is the base dispatch ratio and nothing more.
* **Deferring the per-bytecode `last_instr_pc` store was considered and
  declined.** ~30 readers including `stackwalker`, which runs at cross-thread
  safepoints; the store is to a cache line the frame is touching anyway. Risk
  far exceeds the ~1 ns.
* **Hoisting the frame pointer past the second `&mut thread.frames[frame_idx]`
  was considered and declined.** `interpreter.rs`'s own hoist note records that
  the checked borrow is deliberate - it is what makes the ~150 arms'
  `let _ = frame;` discipline compile-time-enforced - and both computations
  derive from the same base in the same basic block after a null check that
  already proves the index in range, so the bounds check is plausibly folded
  already. Needs a disassembly, not a profile.

### The probes

`probes/FieldShape.java`, `probes/BackEdge.java`, `probes/Arity.java`,
`probes/Dispatch.java`, `probes/RetTag.java`, `probes/DecodedShare.java`,
`probes/ClassLoadShape.java`. All
self-time, interleave their arms in both directions on alternating rounds, and
report min-of-N. Every one of them carries a control arm in the same process,
which is what makes them usable on a host at 90% CPU - the arms move together,
the difference does not.

```bash
javac -d /tmp/probe probes/FieldShape.java probes/BackEdge.java \
    probes/Arity.java probes/Dispatch.java probes/RetTag.java \
    probes/DecodedShare.java

cratonvm --java-home <JDK 25> --nojit -c /tmp/probe FieldShape 1000000 9
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe BackEdge   4000000 9
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe Arity      1500000 9
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe Dispatch   1000000 9
cratonvm --java-home <JDK 25> --nojit -c /tmp/probe RetTag     1500000 9

java -Xint -cp /tmp/probe <Probe> ...            # the reference column
```

`BackEdge` is the one that needs a quiet host: it is a small difference between
two large arms, and one A/B pass of it produced an impossible value (turning
OSR off cannot make a back edge more expensive) purely from load.

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

## The 2026-09-02 second pass: seven findings, and the allocation shape that hid the biggest one

The first pass above priced the interpreter operation by operation and removed
five costs that were fixed per method and paid per operation. This pass read the
code under the three worst rows of that table — instance field access at ~170x,
the invoke at 19-40x, straight-line bytecode at 12.8x — and found seven more of
the same shape. All seven are implemented on `perf/interp-seven-20260902`.

Every number below is an **internal control**: one Azure host at load 13-18
(`free -g` showed 5-13 GB free throughout), three arms in the same process
family, interleaved in both directions across four passes. The three arms are:

| arm | binary | switches |
|---|---|---|
| `base` | `origin/dev` at `209d3825a` | — |
| `new` | this branch | none set |
| `off` | this branch | `CRATONVM_JIT_NO_FIELD_FAST_PATH=1 CRATONVM_JIT_NO_OSR_INLINE_GATE=1 CRATONVM_JIT_NO_INVOKE_FAST_DOOR=1` |

Both binaries were built with `CARGO_PROFILE_RELEASE_LTO=off
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` — the fat-LTO `native-builtins` compile
was OOM-killed twice on this host, once at load 74 and once at `-j 1`. Same
settings for both arms, so the comparison holds; absolute figures are not
comparable to a fat-LTO build.

**`off` is not the same as `base`**, and the difference is the point: three of
the seven items are pure deletions with no kill switch (items 4, 6, 7), so the
`off` arm keeps them. `base → off` therefore measures those three, and
`off → new` measures the three that are switched (items 1, 3, 5). Read the
tables that way.

### 1. Instance field access had no fast path — and the first version never fired

`getfield` / `putfield` reached `op_getfield` / `op_putfield` on every
execution. Per read that handler decoded the receiver into a 16-byte `Value`
through a closure-carrying pop, forwarded it twice, probed the field-site cache
and cloned the `ResolvedField`, read about ten diagnostic gates, hashed the
method name for a JVMTI watchpoint that was not set (item 6), then went through
`VmHeap::get_field` — two quiescence gates, a collector dispatch, a field-index
check, a punned-store watch, a thread-local layout lookup and an
`Arc<CompactLayout>` clone — and finally narrowed the result by descriptor,
forwarded it and pushed a `Value`. Six address probes, two refcount round trips,
one string hash and two representation conversions to load one word.

The fix is the classic quickened field access: the slow handler, on the access
that resolved the field, records the receiver's `(class id, num_slots)` shape
and the field's byte offset in a new per-thread `SiteCache`
(`JvmThread::fast_field_sites`, same key and epoch validation as `field_sites`),
and the dispatch arm compares the next receiver's header against the site and
loads or stores directly. `vm/src/runtime/interpreter/field_fast.rs` carries the
contract; everything it cannot prove verbatim falls back to the full handler,
which refills the site.

**The first version of it measured `fast-field: get hit=0 miss=1801267` — it
never fired once**, and the reason is worth recording because it is not
discoverable from the class side. A ZGC object body is one of two shapes:

* a **compact** body, fields packed at their natural widths at the offsets of
  the class's registered `CompactLayout`, marked `GC_FLAG_COMPACT`;
* a **legacy** body, one 16-byte tagged `Value` cell per field, no flag.

`CRATONVM_DBG_LAYOUT=1` says the probe's classes *have* compact layouts
(`FieldShape$Base cid=468 body=8 refs=0 fields=1`), which is what sent the first
version down the compact path only. But layouts are consulted by
`GarbageCollector::alloc_object`, and the interpreter does not allocate through
it: `interpreter::alloc_object_shared` calls `VmHeap::try_alloc_object`, and
`ZgcRealHeap::try_alloc_object` sizes the allocation `num_fields * SLOT_SIZE`
and **never calls `set_compact_shape`**. So on this collector essentially every
object the interpreter allocates is legacy, whatever its class's layout says.
The arms now carry both shapes, chosen per site by the receiver's compact flag
and re-checked on every access. (Whether `try_alloc_object` *should* build
compact bodies is a separate question with a much wider blast radius; it is not
touched here. The two halves are self-consistent today — a legacy body carries
no flag, and every reader checks the flag before striding a body.)

Engagement after the fix, `probes/FieldShape.java`, 200k x 2:

```
fast-field: get hit=800869 miss=39 fill=57 put hit=800993 miss=119 fill=107 unusable=0
```

`probes/FieldShape.java` at 300k x 5, ns per read+write pair over its own
no-field control, four interleaved passes:

| arm | own field | inherited field | static field (control) |
|---|---|---|---|
| base | 311 / 307 / 281 / 273 | 316 / 310 / 278 / 266 | 138 / 146 / 121 / 125 |
| **new** | **68 / 68 / 65 / 64** | **58 / 78 / 63 / 64** | 94 / 116 / 107 / 109 |
| off | 272 / 280 / 297 / 298 | 274 / 268 / 286 / 272 | 120 / 109 / 125 / 110 |

4/4 with no overlap: `new`'s worst pair (68) is a quarter of `off`'s best (272).
**`getstatic` / `putstatic` are untouched by this change and are the internal
control** — they move within noise across all three arms while the instance
arms move 4.3x. The `off` arm sitting with `base` is what says the kill switch
restores the old path.

The same file gives primitive `*aload` / `*astore` an inline arm (header kind,
element type and length checked directly, element read or written at its
address). Reference arrays keep the barrier-aware path.

### 2. Frame construction copied every argument twice and zero-filled the stack

Arguments went `CompactValue → Value → CompactValue` through a 256-byte
`[Value; 16]` on every invoke, and `ValueStack::from_pooled` cleared and
zero-filled `max_stack + 24` words of a pooled buffer whose contents no reader
looks at above `len` (the GC scans, the pointer rewrite and freeze/thaw all stop
at `len`, and a slot below `len` is written by a push first).

`Frame::new_pooled_cached_compact` now takes `(slot, descriptor tag)` pairs read
straight off the caller's operand stack and writes each once, with the same
category-2 filler `copy_args_to_locals` wrote, so the locals are bit-identical
to the general path's. `from_pooled` hands a long-enough buffer over as it is
and grows (zero-filling the tail) only a short one. Measured as part of item 3,
which is the only caller of the compact constructor.

### 3. Per-call constants were recomputed in the virtual dispatcher

Every warm `invokevirtual` / `invokeinterface` hit cloned the cache entry (two
`Arc` increments and two decrements, because `RedefineGate` carries its own
`Arc<AtomicU32>`), decoded every argument to run an interception chain whose
every question is a constant of the callee, took a class-manager read lock and
did a string prefix compare to ask whether the receiver is a `java.util` class,
and paid a sharded read lock plus a hash lookup for the invocation counter.

`execute_invokevirtual_fast_door` now handles the monomorphic bytecode hit with
a borrowed entry, a header compare on the receiver, the callee's memoized
intercept shape (the three name-matched intercepts set a fourth bit in it), the
`NativeCallSite` memo for "has a registered native", a lock-free per-class-id
bitmap (`class_is_java_util`, set at definition) for the `java.util` question,
and an invocation counter on the `CachedBytecodeMethod` itself
(`interp_invocations`, one relaxed `fetch_add`) credited to the profile store
sixteen calls at a time, so the census and `hot_but_stuck_in_interpreter` still
see every call. A trivial instance getter answers from the quickened field site
of its own class without a frame. Anything the door cannot prove returns `None`
with the operand stack untouched and `execute_invokevirtual_cached` runs exactly
as before.

`probes/Dispatch.java` at 200k x 5, ns/iteration, four interleaved passes:

| arm | base | new | off |
|---|---|---|---|
| `nocall` (control) | 65 62 61 64 | 50 52 51 51 | 52 51 48 51 |
| `static0` (control) | 265 266 256 257 | 228 245 234 258 | 261 233 237 239 |
| `special1` (control) | 487 475 444 446 | 419 425 414 492 | 457 430 397 406 |
| **`virtual1`** | 472 493 470 478 | **246 236 240 258** | 469 431 456 466 |
| **`ifaceInherited`** | 537 571 495 498 | **262 249 249 276** | 506 473 432 455 |
| `iface1` | 517 543 489 452 | 280 244 259 **420** | 493 461 448 468 |

`virtual1` and `ifaceInherited`: 4/4, no overlap, **~215 ns and ~200 ns off a
call**. `iface1` is 3/4 clean with one 420 outlier that still sits below the
`off` arm's best (448) — recorded, not smoothed.

**`invokestatic` and `invokespecial` are not wired to the door and are the
internal controls**: `static0` and `special1` overlap across all three arms.
(Correction, third pass: `special1` is not an `invokespecial` — `javac` 25
emits `invokevirtual` for a private instance method, and the door declined it
because its target caches as `Bytecode` rather than `VirtualBytecode`. It was
a genuine control for *this* pass, but for the wrong reason. See the third
pass below.)
`probes/Arity.java` reproduces it independently — `virtual1` base 465/485/474,
off 443/491/467, new 246/249/269 — while its `static0..static6` ladder shows no
door effect at all.

### 4. The loop top ran nine branches before reading the opcode

Two of them are gone. The two pending-signal slots (`pending_java_exception`,
`pending_runtime_error`) were two `Option` discriminant loads and two branches
per bytecode; they are now one `if a.is_some() | b.is_some()` — a single test of
the OR — with the runtime arm moved above the safepoint poll, which is what the
Java arm already did and which strictly improves its GC-pin argument (the
throwable `throw_runtime_error` allocates is stored into a handler frame before
any safepoint can observe it).

And the stack-dump hook, an atomic load per bytecode, is now skipped wholesale
on a hoisted bool. Nothing can set `stack_dump_requested` unless a watchdog or
sampler was armed, and both are armed before Java starts running, so a run with
neither — every run that is not being debugged — pays nothing.
`SharedVm::arm_stack_dump_watch` is a one-way latch set at **spawn** time rather
than fire time, because the thread a watchdog exists to photograph is by
definition one that has been inside a single `execute_frame` for a long while
and would never re-read a per-frame gate.

### 5. Every backward branch made an out-of-line call — and this one did not separate

`try_osr_with_backoff` read three gates and linearly scanned
`osr_attempt_counts` on every back edge before deciding to do nothing. It cannot
do anything until `Frame::backward_count` reaches the smallest threshold
`should_try_osr` accepts, so the four back-edge sites now compare against that
floor inline (hoisted per `execute_frame`; `u32::MAX` on a virtual thread or
with OSR off, `0` while the arrival trace is armed so it keeps its rate) and
call out only past it.

**It did not separate.** `probes/BackEdge.java` at 3M x 7, derived per-back-edge
cost in ns: new 14.0 / 11.1 / 11.1 / 12.1, off 17.6 / 14.2 / 9.0 / 15.9, base
13.2 / 14.2 / 14.6 / 10.8 — three overlapping distributions. That metric is a
small difference between two large arms, and after the first pass's back-edge
poll gate the remaining call was already cheap. It is kept as a deletion with a
kill switch, not recorded as a win.

### 6 and 7. Fixed costs and diagnostic density, measured together

* The four field opcodes hashed the method name (`synth_method_id`, an FNV over
  the bytes) *before* testing whether any JVMTI field watchpoint exists. The
  hash is now inside the gate.
* `check_vacated_compact` on every operand-stack push, and `load_and_forward` /
  `get_field` on every heap access, read `vacated_frames_enabled` through a
  `OnceLock`; it is a relaxed byte load now, as is `invoke_phases::on`, which
  the value-return arm reads five times per return.
* The five consolidated diagnostic blocks of `op_getfield` / `op_putfield` —
  the stray-stack probe, the `any_field_diag` union, the field watch, ~370 lines
  with their own closures and backtraces — moved verbatim into
  `#[cold] #[inline(never)]` helpers. The handler keeps one gate load per block
  and none of the code.

These three have no kill switch (they are deletions), so they are what
`base → off` isolates, on the two arms that are pure straight-line bytecode:

| probe / arm | base | off | new |
|---|---|---|---|
| `Dispatch` `nocall` ns/iter | 65 62 61 64 | 52 51 48 51 | 50 52 51 51 |
| `Arity` `nocall` ns/iter | 57 66 60 | 51 47 51 53 | 50 50 50 51 |
| `BackEdge` `tight` ns/element | 61.3 63.0 64.6 60.4 | 53.4 50.6 50.9 54.3 | 52.0 48.1 51.0 50.6 |

4/4 with no overlap on all three (`base`'s best is worse than `off`'s worst in
every row): **~15-18% off straight-line interpreted bytecode**, from removing
one atomic load and one branch per dispatch and shrinking the two field
handlers. `new` and `off` agree, which is the expected shape — the switched
items do not touch these arms.

### What did NOT separate, so nobody re-derives it

* **The OSR inline gate** (item 5), above.
* **The per-extra-argument slope.** `probes/Arity.java`: base 29.9/29.0/30.2,
  new 27.7/21.9/29.6/27.7, off 24.6/30.1/22.0/21.6. The compact argument
  transfer serves the virtual door, and this probe's slope is computed from the
  `invokestatic` ladder, which the door does not handle. The first pass recorded
  the same non-separation for the same reason.
* **`invokestatic` and `invokespecial`** are untouched. `static0` at ~250 ns and
  `special1` at ~430 ns are now the two worst interpreted call shapes by a wide
  margin, and they are the obvious next target: `0xb8` has a cached dispatcher
  of its own (`execute_invokestatic_cached`) that pays the same per-call
  constants the virtual door now memoizes. **Taken by the third pass below**,
  which also found that `special1` was never an `invokespecial`.

### Still open

* **The frame arena.** Item 2 removed the argument round trip and the stack
  memset; the frame still owns four heap buffers and is moved by value on push.
  A single per-thread value arena in which the callee's locals overlap the
  caller's outgoing arguments is the structural change that removes the rest of
  the frame-lifecycle share, and it touches every reader of `Frame::locals` and
  `ValueStack` — the GC scans, freeze/thaw, deopt. **Re-scoped with
  measurements by the fourth pass below: the cost is the per-frame object
  churn, not the filling of the buffers.**
* **Polymorphic sites** thrash the fast field site and the invoke door alike;
  both fall back on every receiver change, which is the pre-existing shape of
  the monomorphic inline cache.
* **`ZgcRealHeap::try_alloc_object` builds legacy bodies** while the class
  system builds compact layouts for the same classes. Making the TLAB path
  compact-aware would halve object footprint and let the packed-width field arm
  serve everything, but it changes the layout of every ZGC-allocated object.

### The probes

`probes/FieldShape.java`, `probes/Dispatch.java`, `probes/Arity.java`,
`probes/BackEdge.java`. All self-time, interleave their arms in both directions
on alternating rounds, and report min-of-N; each carries a control arm in the
same process.

```bash
javac -d /tmp/probe probes/FieldShape.java probes/Dispatch.java \
    probes/Arity.java probes/BackEdge.java

for arm in base new off; do
  case $arm in
    base) BIN=<dev binary>; ENV= ;;
    new)  BIN=<branch binary>; ENV= ;;
    off)  BIN=<branch binary>; ENV="CRATONVM_JIT_NO_FIELD_FAST_PATH=1 \
          CRATONVM_JIT_NO_OSR_INLINE_GATE=1 CRATONVM_JIT_NO_INVOKE_FAST_DOOR=1" ;;
  esac
  env $ENV $BIN --java-home <JDK 25> --nojit -c /tmp/probe FieldShape 300000 5
done
```

`CRATONVM_DBG_FIELD_SITE=1` adds the `fast-field:` census and names the first
few reasons a site could not be quickened.

## The third pass: `invokestatic`, and the call shape the probe was mislabelling

The second pass left `invokestatic` (~250 ns) and what it called
`invokespecial` (~430 ns) as its two untouched controls, and named them the
obvious next target. This pass takes both. Same instrument: one Azure host at
load 13-19, three arms interleaved in both directions over four passes, with
control arms the change cannot reach.

| arm | binary | switches |
|---|---|---|
| `base` | `origin/dev` at `46ddd2bd4` | — |
| `new` | this branch | none |
| `off` | this branch | `CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR=1` |

`vm/src/runtime/interpreter/invoke_fast.rs` holds the shared machinery — the
verbatim argument read, the frame push, the per-callee constants and the
batched invocation credit — plus the two doors. Kill switch:
`CRATONVM_JIT_NO_NONVIRTUAL_FAST_DOOR` (`CRATONVM_JIT=-nonvirtual-fast-door`);
the virtual door keeps its own.

### A correction to this page: `Dispatch.special1` is not an `invokespecial`

The second pass reported "`invokespecial` at ~430 ns" and used `special1` as a
control on the grounds that the virtual door could not reach it. Both halves
were wrong about the same fact:

**`javac` 25 emits `invokevirtual` for a private instance method.** JEP 181
(nestmates) removed the need for `invokespecial` there, and
`javap -c -p Dispatch` shows `invokevirtual #24 // Method p1:(I)I` in `cP1`.
So `special1` measures an `invokevirtual` whose target is *not* virtually
dispatched — the resolver caches it as `CachedInvokeTarget::Bytecode`, with no
receiver class and no vtable slot, and `execute_invokevirtual_fast_door`
accepts only `VirtualBytecode`. That is the whole explanation for the 430 ns
against a virtual call's 245 ns, and it means the second pass's `special1`
column was measuring a gap its own door had left open rather than a call shape
out of reach.

The number stands; the label did not. A door that covers "the target is a
fixed method" now serves both `invokespecial` and that case, and `0xb6` tries
it after the virtual door declines.

### What the doors remove

Both dispatchers pay the three costs the virtual door removed: the cache entry
is cloned (two `Arc` increments and two decrements, `RedefineGate` carrying its
own `Arc<AtomicU32>`), every argument goes `CompactValue -> Value ->
CompactValue` through a stack array, and `invokestatic` takes a sharded
`RwLock` read plus a hash lookup for the invocation counter **on every call**.
A door borrows the entry, transfers arguments verbatim into the callee's
locals, and counts on the `CachedBytecodeMethod` itself, folding sixteen calls
at a time into the profile store so the census still sees every call.

`invokespecial`'s door deliberately does **not** count: neither arm a cached
`invokespecial` can land in has a tier-up block (the `VirtualBytecode` arm's is
guarded by `!is_special`, the `Bytecode` arm has none), and adding one would
widen which methods reach the optimizing tier. A door must not change tier-up
policy on its way past.

### Engagement first

`CRATONVM_DBG_FIELD_SITE=1` now prints a door census beside the site caches.
`probes/Dispatch.java` at 150k x 2 on the measured binary:

```
door: static hit=900168 miss=378 special hit=300710 miss=802
```

99.96% and 99.7%. **Two of this pass's three findings came from that counter,
not from the clock**, and neither would have been visible in a timing run:

* the first `invokespecial` door measured `special hit=0 miss=997`, declining
  on `loader_aware_resolution`, which is **default-on** — not the rare mode it
  was taken for. It now performs the same owner re-check the general path does
  (early-returning on a one-way latch for an ordinary application).
* the census then still showed `hit=463` against 450 000 calls on the arm that
  should have been all hits, which is what sent me to `javap` and the
  nestmates finding above.

### The numbers

`probes/Dispatch.java`, 200k x 5, ns/iteration, four interleaved passes:

| arm | base | new | off |
|---|---|---|---|
| `nocall` (control) | 51 49 65 62 | 52 52 51 49 | 51 60 61 52 |
| `virtual1` (control) | 250 286 278 266 | 246 273 233 282 | 248 251 236 239 |
| `ifaceInherited` (control) | 259 263 316 267 | 264 301 234 356 | 257 266 245 257 |
| **`special1`** (private target) | 430 454 460 506 | **245 239 250 269** | 460 480 427 455 |
| **`static0`** | 234 262 279 278 | **212 231 200 211** | 243 305 272 247 |
| **`static1`** | 275 293 286 296 | **223 239 219 230** | 272 326 265 267 |
| **`static4`** | 331 382 363 363 | **260 254 258 278** | 348 359 335 346 |

4/4 with no overlap on every test arm — `new`'s worst is better than `off`'s
best in each case. **A call to a fixed target falls from ~455 ns to ~250 ns**,
level with a virtual call; a four-argument static call loses ~85 ns.

`probes/Arity.java`, 800k x 5, is the independent confirmation and shows where
the static gain comes from:

| arm | base | new | off |
|---|---|---|---|
| `nocall` (control) | 54 55 60 52 | 52 55 59 49 | 56 52 55 53 |
| `virtual1` (control) | 234 252 235 272 | 277 264 276 246 | 277 271 293 309 |
| `static0` | 248 255 274 268 | **211 209 226 199** | 249 256 265 280 |
| `static1` | 269 271 328 287 | **234 235 219 254** | 313 277 311 277 |
| `static2` | 327 323 302 297 | **254 249 257 258** | 346 313 325 302 |
| `static4` | 373 358 383 347 | **263 283 260 281** | 372 382 363 377 |
| `static6` | 401 406 437 389 | **295 296 294 312** | 422 417 431 461 |

4/4, no overlap, on all five. The gap widens from ~50 ns at zero arguments to
~125 ns at six — **about 12-15 ns per argument**, which is the round trip the
verbatim transfer deletes and is the same slope the first pass measured and
could not then remove.

### Where the interpreted call now stands

Every interpreted call shape that reaches a cached bytecode target is now
served by a door, and they land within ~50 ns of each other:

| shape | before this pass | after |
|---|---:|---:|
| `invokestatic`, 0 args | ~250 | ~210 |
| `invokestatic`, 4 args | ~360 | ~265 |
| `invokevirtual` | ~245 | ~245 |
| `invokeinterface`, inherited | ~260 | ~260 |
| fixed target (private / `invokespecial`) | ~455 | ~250 |

What remains between that and HotSpot's ~4 ns is the frame itself, which is
the open structural item both earlier passes name: four pooled buffers per
frame and a by-value push, where a per-thread value arena would let the
callee's locals overlap the caller's outgoing arguments.

### The probes

Unchanged: `probes/Dispatch.java` and `probes/Arity.java`, interleaved in both
directions with `nocall` and the arms the change cannot reach as controls.
`CRATONVM_DBG_FIELD_SITE=1` prints `door: static hit/miss special hit/miss`
and names the first dozen reasons a door declined — **read it before reading a
clock**, on the evidence of this pass.

## The frame arena, re-scoped: it is not the fill

Every pass on this page has closed by naming the same open item — "the frame
still owns four heap buffers and is moved by value on push; a per-thread value
arena is the structural change that removes the rest of the frame-lifecycle
share" — and every pass has described that share as the *buffers and their
filling*. This pass went to take it, measured first, and found the description
wrong in a way that matters for whoever takes it next.

### The instrument

`probes/FrameShape.java` (generated by `tools/gen_frameshape.py`, so its deep
expressions stay balanced) holds the executed body constant and varies only
`max_locals` / `max_stack`. Each big arm declares its locals, or builds its
deep expression, inside a branch the caller never takes, so javac raises the
frame size for the whole method while the executed path stays the same few
bytecodes as the small arm:

| arm | `max_stack` | `max_locals` |
|---|---:|---:|
| `small` | 1 | 1 |
| `locals48` | 2 | 49 |
| `stack32` | 32 | 1 |
| `both` | 48 | 49 |

The delta between arms is the per-frame sizing and fill cost and nothing else.
A third argument runs **one arm only**, which is what makes the process-wide
rdtsc phase counters (`CRATONVM_DBG=invoke-phases`) readable per arm — without
it every run mixes all five arms and the phases are identical by construction.

### What a bigger frame actually costs

Per-arm, one arm per process, `--nojit`, the general dispatcher (doors off) so
the phase counters are charged:

| phase (cycles/call) | `small` | `locals48` | `stack32` |
|---|---:|---:|---:|
| ic_lookup | 71.5 | 66.2 | 61.4 |
| guards | 37.4 | 37.3 | 34.6 |
| args | 57.1 | 57.0 | 54.6 |
| **frame_build** | **114.3** | **128.3** | 109.8 |
| frame_push | 38.5 | 36.7 | 35.7 |
| ret_recycle | 70.9 | 68.7 | 65.8 |
| measured total | 461.5 | 466.6 | 429.3 |

Two things fall out, and both were surprises:

1. **`frame_build` is the only phase that moves with frame size at all** — and
   it moves by **14 cycles for 48 extra local slots**, about 0.3 cycles per
   slot. That is already a vectorised fill. The other 100 cycles of
   `frame_build` do not care how big the frame is.
2. **The operand stack costs nothing extra.** `stack32` is not slower than
   `small` anywhere. That is the second pass's `from_pooled` change working:
   a pooled slot buffer that is already long enough is handed over as it is,
   so only the one-byte `kinds` array is still sized per call.

So the frame-lifecycle share is real and large — `frame_build` 25% plus
`ret_recycle` 15% is **40% of an interpreted `invokestatic`** — but it is
**size-independent**. It is not the filling of the buffers. It is the churn of
the frame *objects*: two pooled `(Vec, Vec)` tuples popped and pushed back,
four `Vec` headers taken and reinstalled, a `ValueStack` built, a ~220-byte
`Frame` constructed and then moved into the frame stack, and the mirror image
of all of it on return. Roughly 700 bytes of struct shuffling per call, none of
which depends on `max_locals`.

### The change that did not separate, recorded so it is not retried

On that first (wrong) reading I rebuilt the locals construction as a single
pass — size both buffers once with the filler already in place, write the
arguments by index — replacing `clear()` + a `push()` per argument +
`resize()`. Same end state, one walk of the buffer instead of three, behind
`CRATONVM_JIT_NO_FRAME_FILL_FAST`.

Six interleaved passes of `FrameShape` at 400k x 5 on the Azure host, two arms
of one binary, cost in ns of 48 extra local slots:

```
fill fast ON   43.3  43.9  35.6  32.8  56.4  30.6     median 39.5
fill fast OFF  33.8 110.7  40.0  37.1  34.0  54.5     median 38.6
```

No separation. And the honest tell is in the control: the `stack32` arm, which
that change **cannot touch**, swung just as widely (ON 28.5-73.9, OFF
29.1-115.6). The effect, if any, is below what this probe resolves on this
host, which is unsurprising once the phase table above says the whole
size-dependent cost is 14 cycles.

**It was reverted.** It bought nothing measurable and paid for it in `unsafe`
(`set_len` plus a manual fill) and in a second copy of the build kept behind a
switch — the opposite of the trade that justified keeping the unmeasured
deletions of earlier passes, which removed code and locks rather than adding
them. The probe and its generator are kept; they are the reusable part.

### What this means for the arena

The arena is still the right change, but for a different reason than this page
has been giving:

* **Not** because the buffers are filled per call. That is 14 cycles.
* **Because a frame is four heap buffers and a 220-byte struct that is
  constructed, moved into the frame stack, and taken apart again on every
  call.** An arena replaces all of it with a bump of a per-thread offset and a
  `(base, len)` pair in the frame — and, at the same time, lets the callee's
  locals *overlap* the caller's outgoing arguments, which deletes the argument
  copy the fast doors still perform.

The two measurements a future attempt should take before writing any code:

1. `frame_push` is 38 cycles and `ret_recycle` 71. Emplacing the `Frame`
   directly into the frame stack's slot (rather than constructing it and
   moving it in) targets the first; the pooled-buffer hand-off targets the
   second. Both are size-independent, both are worth more than the fill.
   **The second is taken by the section below**, which retires frames instead
   of destroying them: `ret_recycle` 19.6 cycles to 9.0, and 55-90 ns off
   every interpreted call shape.
2. The GC scan is what forces the locals buffer to be initialised at all
   (`scan_local_objects` walks the whole buffer and would otherwise root a
   previous frame's dead references). An arena has the same obligation, and
   the way out is the one HotSpot takes — consult the method's stack map for
   which locals hold references at the current bci — not more zeroing. The
   verifier's type maps are already in this VM and are described elsewhere in
   these notes as "the independent oracle"; that is the piece to build first,
   because it is what makes an arena legal, not merely fast.

### The half of the arena that needs no oop maps: retire the frame, keep the slot

The re-scoping above splits the frame cost in two, and only one half waits on
precise oop maps:

* the **fill** — initialising `locals` so the GC scan cannot see a previous
  frame's dead references. 14 cycles. Needs oop maps to remove.
* the **churn** — two pooled `(Vec, Vec)` tuples popped and pushed back, four
  `Vec` headers taken and reinstalled, a `ValueStack` rebuilt, a ~220-byte
  `Frame` constructed and moved into the frame stack, and the mirror image on
  return. The other ~140 cycles. Needs nothing.

This takes the churn.

`FrameStack` now separates its **logical depth** from its allocation.
`buf.len()` is a high-water mark; the slots above `depth` hold *retired*
frames whose four buffers stay exactly where they are. A return retires the
frame where it lies (`retire_top`), and the next call at that depth is rebuilt
in its buffers by `Frame::reset_cached_compact` — no pool round trip, no `Vec`
headers moved, no `Frame` moved.

Nothing above `depth` is live. Every accessor on the type is bounded by it —
`len`, `iter`, `last`, `get`, `as_slice`, `Deref`, `Index`, `IntoIterator` —
so the GC root scan, the stack walker and every `frames[i]` see exactly the
frames they saw before. A by-value push still harvests the retired slot into
the thread pools first, so the pooled constructors keep the recycling they
have always had.

All three fast doors now share one push (`invoke_fast::push_frame_verbatim`).
That is not tidying: the virtual door had carried its own copy of that tail
since the second pass, and the copy is precisely why the first measurement of
this change showed the static arms improving and `virtual1` flat.

`probes/Dispatch.java` at 200k x 5, ns/iteration, **six** interleaved passes,
two arms of one binary (`CRATONVM_JIT_NO_FRAME_SLOT_REUSE` off and on), Azure
at load ~10:

| arm | reuse ON | reuse OFF |
|---|---|---|
| `nocall` (control) | 47 36 30 29 29 29 | 52 30 44 29 29 29 |
| `static0` | 153 162 146 132 129 109 | 220 190 181 218 200 145 |
| `static1` | 144 188 149 169 144 117 | 241 224 192 239 213 151 |
| `static4` | 181 240 158 179 172 133 | 247 227 280 294 224 170 |
| `virtual1` | 147 158 202 147 168 125 | 248 244 209 218 216 160 |
| `special1` | 193 162 146 140 154 126 | 226 200 239 253 225 161 |
| `iface1` | 231 227 188 224 160 127 | 320 338 228 274 245 162 |
| `ifaceInherited` | 144 173 141 199 205 125 | 288 212 265 246 247 164 |

**Every call shape improves in all six passes, pairwise**, by 55-90 ns —
roughly a third off an interpreted call — while `nocall`, which pushes no
frame at all, is flat. `CRATONVM_DBG=invoke-phases` on the same binary puts
the saving where the design says it is: `ret_recycle` 19.6 cycles → 9.0, and
the whole call 467 → 413.

#### Two bugs the tests and the control caught first

Both are worth recording because neither would have shown up as a wrong
answer, and one of them was invisible until the control arm was believed.

1. **`MethodEntry` fired twice.** Splitting the event out of
   `push_frame_and_fire_entry` so the reusing push could fire it too left the
   original block in place. Three JVMTI unit tests failed immediately.
2. **The switch-off arm poisoned the buffer pools.** On the pooled path the
   husk's buffers have *already* been harvested, so
   `harvest_retired_slot` handed the pools four **zero-capacity** `Vec`s;
   every later frame build then popped an empty buffer and reallocated from
   scratch. It measured as `ret_recycle` at **427 cycles against 17** and a
   2-3x regression — in the arm that exists to be the control. A control that
   is slower than the thing it controls is not a baseline, and the first A/B
   run with it was discarded rather than reported.

   The guard is one capacity test, and the lesson generalises: **when a change
   moves *where* a resource is released, check every path that releases it,
   including the one the kill switch turns back on.**

#### The emplace, and why it needed its own switch to be visible

The slot-reuse section above left one item: when **no** slot is retired, the
door still built a `Frame` on the Rust stack and `push` moved ~220 bytes of it
into the very slot it could have been written in.

`Frame::new_pooled_cached_compact`'s buffer build is now factored into
`build_cached_compact_parts`, shared with `FrameStack::emplace_cached_compact`
so the by-value constructor and the emplace cannot drift. The emplace writes
the struct literal through `ptr::write` **in the same function as the write**,
so the destination is known at the point of construction and there is no
intermediate to copy from; only the three buffer handles the caller just took
from the pools travel.

**That path is cold by default, and that is the whole measurement problem.**
After slot reuse, a warm loop retires and rebuilds the same slot forever and
never emplaces at all; the first call at each depth is one call in millions.
So the change cannot be measured in the configuration that ships — not because
it does nothing, but because the arm it governs almost never runs.

`CRATONVM_JIT_NO_FRAME_SLOT_REUSE=1` forces it: with reuse off, the recycle
harvests and trims, so every door call finds no retired slot and takes the
emplace. That plus a switch on the emplace itself
(`CRATONVM_JIT_NO_FRAME_EMPLACE`) isolates exactly the 220-byte move inside one
binary. `probes/Dispatch.java` at 200k x 5, six interleaved passes, Azure at
load 3.4 (`nocall` dead flat at 27-28 ns, which is what says the host was
quiet):

| arm | emplace | no-emplace |
|---|---|---|
| `nocall` (control) | 28 27 28 27 27 27 | 27 27 27 29 27 28 |
| `static0` | 142 135 137 134 134 134 | 149 146 144 153 143 147 |
| `static1` | 146 140 141 140 140 140 | 154 152 150 160 150 150 |
| `static4` | 174 157 157 158 158 161 | 174 169 168 170 168 170 |
| `virtual1` | 159 147 147 147 147 150 | 164 161 157 157 156 158 |
| `special1` | 153 147 148 146 147 149 | 157 157 156 162 155 155 |
| `iface1` | 164 150 149 159 156 151 | 164 170 163 170 160 159 |

**~9-12 ns per call, 6/6 pairwise on `static0`/`static1`/`virtual1`/`special1`
(5/6 and a tie on `static4` and `iface1`), and the no-call control does not
move at all.** `static0` and `static1` do not even overlap: the emplace arm's
worst pass beats the other arm's best.

So the emplace is worth what the frame move costs, on the path where the frame
move happens. What it buys in a default run is the first call at each depth
plus every call in a workload whose retired slots keep being destroyed by
interleaved by-value pushes — and it makes `CRATONVM_JIT_NO_FRAME_SLOT_REUSE`
a much cheaper fallback than it was.

**What it does not cover, deliberately.** The general dispatchers still push by
value through `push_frame_and_fire_entry`, and because that harvests and trims
the slot it would have reused, *every* non-door call takes the by-value build.
Converting those is the same change again, but their call sites interleave the
monitor-enter, the frame trace and the JVMTI entry event around the push in
three different orders, and reordering that is not a change to make on the way
past. It is the obvious next increment, and it is worth more than this one
because it is not a cold path.

#### What is left

The fill, which is the oop-map half, and the general dispatchers' by-value
push (see the note that closes the emplace section above).

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

### The general dispatchers, on a day the doors went dark

The section above ends by naming what it did not cover. Between then and this,
`f74e88d8a` made the monomorphic invoke fast door **default-OFF** — it returns
wrong answers on `TestRandomMapOps`
(`known-issues/h2/bug-testrandommapops-deterministic-1810-null-20260903.md`) —
and because `nonvirtual_fast_door_on` is `invoke_fast_door_on && ...`, the
`invokestatic` and `invokespecial` doors went off with it.

So this is no longer an increment. **The general dispatchers are the
interpreted call path**, and everything the last three passes built was
reachable only through a door nothing opens. Worse than not helping: a general
call went through `push_frame_and_fire_entry`, which **harvests and trims** the
retired slot before pushing, so it destroyed the slot the next call would have
reused. Frame-slot reuse was not merely unused on the default path — it was
being actively undone, once per call.

`CRATONVM_DBG_FIELD_SITE=1` says it in one line, and says the same thing with
and without the door kill switches, which is how the door state was noticed at
all:

```
door: static hit=0 miss=0 special hit=0 miss=0 | install: reuse=2800557 emplace=447 byvalue=0
```

#### One install, three paths

All five general pushes — four in `execute_invokevirtual_cached` /
`_vtable_fast`, one in `execute_invokestatic_cached` — now go through
`install_cached_frame`, which is the ladder the doors already had:

1. rebuild the retired slot at this depth (`Frame::reset_cached_value`),
2. emplace into the next slot when none is retired,
3. build by value and move it in — what `CRATONVM_JIT_NO_FRAME_EMPLACE`
   restores, taking the harvest with it, so that arm is the old behaviour whole.

The doors transfer arguments verbatim as `(slot, tag)` pairs; a general
dispatcher has already decoded to `Value`s by the time it knows the callee
shape, so the two differ in that and nothing else.
`build_cached_value_parts` and `build_cached_compact_parts` produce the same
`CachedCompactParts`; `reset_cached_value` and `reset_cached_compact` share
`reset_cached_tail`; four unit tests assert that a rebuilt slot, an emplaced
slot and a by-value push are indistinguishable field for field.

#### What it costs, priced in cycles because the host would not hold still

Wall clock was unusable: `nocall`, which pushes no frame, swung 27-60 ns and
three other sessions had `cratonvm` at ~100% CPU on the same host. One pass in
twelve had a quiet control in all three arms. That run was discarded rather
than mined.

`CRATONVM_DBG=invoke-phases` counts rdtsc cycles inside the process over ten
million calls, and it does hold still. `probes/Dispatch.java` 400k x 9, **eight
interleaved passes**, three arms — `on`, `off` (the kill switch) and `dev`,
which is there because the switch cannot reach the refactor around it:

| cyc/call | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |
|---|---|---|---|---|---|---|---|---|
| `on` | 604 | 643 | 650 | 659 | 662 | 650 | 647 | 634 |
| `dev` | 686 | 724 | 735 | 753 | 747 | 733 | 739 | 728 |
| `off` | 709 | 749 | 760 | 803 | 762 | 758 | 775 | 757 |

**8/8 pairwise against dev, 11-13% off a whole interpreted `invokestatic`, and
the two do not overlap**: the worst `on` pass (662) beats the best `dev` pass
(686). User CPU time over ten separate passes agrees more coarsely — 8/10, mean
7.40 s against 8.08 s.

The same run says where it went. `frame_build` and `frame_push` were re-scoped
for this change (there is no boundary between building a frame in place and
pushing it), so read them as one number — the install:

| corrected cycles | `frame_build` | `frame_push` | install |
|---|---|---|---|
| `on` | 11-21 | **0.0** | **11-21** |
| `dev` | 50-58 | 27-36 | 77-94 |
| `off` | 61-72 | 39-47 | 100-115 |

**The install falls from ~85 cycles to ~16**, and ~70 cycles is what the ~85
cyc/call total improvement is made of. `guards` (100-123) and `args` (63-87)
are flat across all three arms in every pass, which is the control inside the
instrument: the change is not supposed to touch them, and it does not.

#### The kill switch is not free, and the third arm is why that is known

`off` is **worse than dev**, by 21-51 cyc/call. The switch restores the
by-value build but cannot undo the refactor around it: `install_cached_frame`
is an out-of-line call where dev inlined the build, plus a census bump. A
two-arm A/B would have reported 15-19% by measuring against a control that is
itself a regression. The honest headline is `on` against `dev`, and the
recovery path this switch offers is a few percent slower than dev rather than
equal to it.

#### The control that licensed shipping this at all

This change puts the doors' in-place install on the path that serves *every*
interpreted call. If the door's wrong answer came from that machinery rather
than from its argument transfer, this would spread a wrong answer from a
default-off path to all of them — and the bug page could not say which half was
at fault, because `-invoke-fast-door` switches off both at once.

Forcing the door back on and disabling **only** the install
(`CRATONVM_JIT_INVOKE_FAST_DOOR=1 CRATONVM_JIT_NO_FRAME_SLOT_REUSE=1
CRATONVM_JIT_NO_FRAME_EMPLACE=1`) still fails 3/3 — same seed, same `op:1033`,
same `(1810, null)`, in 9.3-11.8 s. The install is excluded; the argument
transfer is not. That row is now on the bug page.

#### Two instruments that had gone quiet

* **The reusing push never took the interp-frame census.**
  `FrameStack::push` records `CRATONVM_DBG_INTERP_FRAMES`;
  `push_cached_compact_reusing` did not. After warmup nearly every door call
  reused a slot, so the very methods the census exists to find were the ones
  missing from it.
* **Three diagnostics saw only by-value pushes.** Splitting the JVMTI event out
  for the doors left `SBF-TRACE`, the bytecode dump and the WFLYCTL0079
  dup-call filter reachable only from `push_frame_and_fire_entry`. Nothing
  failed; a Spring boot trace simply stopped seeing any call that took a
  cheaper install. They now live in `fire_entry_and_diagnostics_after_push`
  and every install path runs them.

#### What is left

The three `Frame::new_pooled_cached` sites in `jit/helpers.rs` — the JIT's
interpreted-callee path — still build by value, but they hand the frame to
`execute_prebuilt_frame` rather than pushing it, so converting them is a change
to that function's contract and not this one. And the locals **fill** is still
the precise-oop-map half.

#### Addendum, same day: the doors came back, and this section overstates its reach

Written hours apart, and the second half corrects the first.

`f74e88d8a` turned the monomorphic invoke fast door off on 2026-09-03 and the
section above was measured in that configuration — doors dark, every
interpreted call through the general dispatchers. **The door went default-ON
again the same day** (`9f356eea4`, *"the invoke fast door never recorded its receiver — root cause,
repaired"*; the code comment reads *"the wrong answers this
door produced were the receiver profile it was not recording, and it records it
now"*), and the H2 wrong answer that justified switching it off is fixed and its
page retired to
`testrandommapops-deterministic-1810-null-FIXED-20260904.md` — the
root cause was the guarded-inline native screen asking the declaring class, not
the door's argument transfer at all.

So the claim above that "the general dispatchers ARE the interpreted call path"
was true when it was measured and is **false on current dev**. What the census
says now, `probes/Dispatch.java` 200k x 2 on dev:

```
door: static hit=1200278 miss=448 special hit=400775 miss=1264 | install: reuse=200066 emplace=29 byvalue=0
```

1.6M calls served by the doors, 200k by `install_cached_frame` — the virtual and
interface shapes the doors decline. The install is still engaged, still reuses
a retired slot on essentially every call it sees (`byvalue=0`), and is still
worth what it measured on the calls it serves. It is about **11%** of this
probe's calls rather than all of them.

The 11-13% cyc/call figure is therefore a per-call number for a general-dispatch
call, measured with the doors off. It is not an 11-13% figure for a workload on
current dev, and nothing here measured that.

Two things this cost, worth naming rather than filing away:

* The three-arm A/B and the engagement census were both right and both
  answered the question as posed. What went stale was the **premise** — which
  path ships — and no arm inside the experiment could have caught that. The
  check that would have is the one that costs a minute: re-read the gating on
  `origin/dev` before writing the framing, not once at the start of the work.
* A door that goes off and back on inside 24 hours is not an unusual event in
  this tree. A section that says "X is the path now" dates itself; one that says
  "X served N of M calls in this census, on this commit" does not.
