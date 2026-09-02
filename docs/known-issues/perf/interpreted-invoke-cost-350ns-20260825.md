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
  constants the virtual door now memoizes.

### Still open

* **The frame arena.** Item 2 removed the argument round trip and the stack
  memset; the frame still owns four heap buffers and is moved by value on push.
  A single per-thread value arena in which the callee's locals overlap the
  caller's outgoing arguments is the structural change that removes the rest of
  the frame-lifecycle share, and it touches every reader of `Frame::locals` and
  `ValueStack` — the GC scans, freeze/thaw, deopt.
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
