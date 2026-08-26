> # CLOSED 2026-08-25 — both of this page's own exit criteria are met, and the third was never this page's work
>
> Retired to `fixed-suite-bugs/tomcat/`. Everything below is the investigation
> as it stood, unedited; this header is the closing measurement and the hand-off.
>
> ## Criterion 1 — no test fails on deploy timing. MET.
>
> `org.apache.catalina.manager.TestManagerWebapp`, driven alone on
> `origin/dev` `5a760532c` + this branch, Azure, load 16–22:
>
> | | CratonVM | HotSpot 25 |
> |---|---|---|
> | `testBug57700` (§ Symptom's first method) | **PASS** | PASS |
> | `testDeploy` (§ Symptom's second method) | **PASS** | PASS |
> | deploy of `/bug57700` | 15 506 / 16 618 ms | 1 363 / 1 466 ms |
>
> ≈ **11x**, against the ~16x this page recorded on 2026-08-07. The class as a
> whole is `Tests run: 4, Failures: 2` — but the two that fail are **not this
> page's**, and that is a measurement rather than an assertion: they fail
> **identically on pristine `origin/dev`**, with the same two names and the same
> count, on a binary built from the unmodified tree.
>
> * `testServlets` — `SocketTimeoutException` at `TestManagerWebapp.java:147`,
>   which is `GET /manager/jmxproxy`. A JMX-proxy servlet failure.
> * `testJsps` — bare `assertTrue` at `:697`, asserting the
>   `/manager/html/sessions` page contains `Sessions Administration`. A manager
>   JSP content failure.
>
> Neither is a deploy, and neither is a timing assertion on one. They belong to
> `known-issues/tomcat/nonpassed-class-census.md`, whose
> `catalina.manager.TestManagerWebapp | 1 of 4` row is updated with today's
> evidence. HotSpot is `OK (4 tests)` in 19.6 s.
>
> ## Criterion 2 — deploy throughput stays within its measured band. MET.
>
> `AnnotationScanCostProbe`, `taglibs-standard-impl`, fat-LTO build, four
> interleaved passes with HotSpot every pass and the arm order reversed on even
> passes, load steady at 23.4–23.7, µs/class:
>
> | pass | 1 | 2 | 3 | 4 | mean |
> |---|---:|---:|---:|---:|---:|
> | CratonVM | 626.0 | 764.3 | 823.7 | 659.8 | **718.5** |
> | HotSpot | 149.2 | 53.6 | 157.0 | 20.9 | 95.2 |
>
> Against the **813.5 µs/class** band this page set on 2026-08-06: inside it,
> and below its midpoint. Take the CratonVM column — HotSpot's own swings 7.5x
> across four passes at this load, exactly as § Measuring this at all says.
>
> ## Criterion 3 — the real work. HANDED OFF, not abandoned.
>
> `docs/known-issues/perf/interpreted-invoke-cost-350ns-20260825.md`.
>
> That page inherits everything on this one that is about the interpreted
> invoke rather than about Tomcat: the ~350 ns figure and its two-arm
> calibration, the control-arm decomposition (frame lifecycle ~24.7%,
> dispatcher ~17.1%, and the rest), the four pieces already taken off it, the
> two untaken levers with the "do not just count them" result attached, the six
> falsified root causes, and § Measuring this at all in full. The eight `.rs`
> comments that cited this page were repointed there, because every one of them
> was citing it for an invoke-path fact and a `docs/internal/…` path in a source
> comment is a dead link the day that folder is dropped.
>
> ## A fourth piece was taken on the way out
>
> `execute_invokevirtual_cached` asked two questions on **every**
> non-`invokespecial` virtual invoke, each of which is a per-cache-entry
> constant, and each of which took a lock to answer:
>
> * "is the receiver class a lambda proxy" — `lambda_proxies.read()` plus a hash
>   probe, to answer *no* for every ordinary class in the program;
> * "is the receiver class the synthetic `AnnotationProxy`" — the class-manager
>   read lock, `get_class`, and an `Arc<str>` compared against a literal. This
>   is the item § The annotation-proxy gate: scoped left specified and unbuilt.
>
> They are now `ClassRealm::is_lambda_proxy_class` and
> `ClassRealm::is_annotation_proxy_class`. The first puts a range test in front
> of the map — proxy ids come only from `alloc_lambda_proxy_id`, whose counter
> is seeded at `LAMBDA_PROXY_ID_BASE`, so an id below the base cannot be in the
> table and the map stays the authority for ids that could be. The second
> memoizes the proxy's `ClassId` per realm, with the **negative** half keyed on
> `class_definition_epoch` so it cannot latch "absent" from before the class was
> minted.
>
> This page's own § scoped a different implementation — a bool computed at
> cache-population time and stored on `CachedInvokeTarget::VirtualBytecode`.
> That was not built, and the reason is worth recording: the flag would have to
> be set correctly at 45 construction sites, and a site that defaulted it to
> `false` would send an annotation proxy down a cached-target path that has no
> bytecode for it — a silent wrong dispatch. The memo has a strictly safer
> failure mode, because the predicate stays in one function and only the
> *caching* is new. Its soundness rests on the same standing property
> `lambda_impl_owner_memo` already relies on and states: CratonVM does not
> unload classes, and in-place `redefine_class` keeps the `ClassId`.
>
> **It did not separate on the wall clock, and that is recorded as a
> non-result** — the `noCall` control arm, which contains no invoke at all,
> moved as much as the test arm on a box at load 33–43. The full table is on the
> successor page. It was kept because it *deletes* work and code from a
> correctness gate rather than adding a cache that has to earn its keep, which
> is the distinction between it and the levers this page reverted.
>
> ## Gates
>
> `cratonvm-vm --lib` 2617/0, `native-io` 526/0, `classloading` 797/0, `types`
> 581/0, `gc` 1687/0, `jit` 2105/0, `native-api` 338/0; `native-builtins`
> 4161 pass / 2 fail, both `shared_secrets_bridge::tests`, **reproduced on
> pristine `origin/dev` with this branch's changes reverted in place**.
> `regression-suite/run.sh` **72 passed, 0 failed**, including a new
> `RAnnotationProxyGate` vector added with this closure: it drives an annotation
> proxy through a WARM call site, alternates that one site between a proxy and a
> hand-written implementation of the same interface, and diffs 20 observables
> against HotSpot. Nothing in the suite drove a proxy through a warm site
> before, so a memo answering `false` one query too early would have gone
> unnoticed.

---

# Webapp deploy: BCEL annotation scan is 226x slower — it never compiles

| | |
|---|---|
| **Status** | OPEN |
| **Severity** | low (was high, then medium) — no test fails on deploy timing since 2026-08-06, and 2026-08-07 measured the remaining gap and re-scoped the exit criteria: this is a tracked throughput item (~16x on a webapp deploy), not a bug. See the 2026-08-07 update. |
| **HotSpot** | PASS |
| **CratonVM** | PASS since 2026-08-06 on the two classes this doc named; still slow (timing only — no wrong results, no crash) |
| **Discovered** | 2026-08-03, after fixing the `seek0`/`ExpandWar` defect that had been masking it (`fixed-suite-bugs/tomcat/testmanagerwebapp-expandwar-seek0-bad-fd-FIXED.md`) |

> **Update 2026-08-11 — every `--stack-sample-ms` profile on this page has
> been read wrong, and the correction moves the target. Also: the 08-11
> `JarFile` fix does NOT move this page, and criterion 2 is re-verified.**
>
> ### The reading error: `pc=0 last_pc=0` is the INVOKE, not the callee
>
> The sampling hook lives at the top of the interpreter's dispatch loop
> (`vm/src/runtime/interpreter.rs`, the `stack_dump_pending()` block). An
> `invokevirtual` resolves, coerces arguments, pushes the callee frame and
> `continue`s — so the **first loop iteration that can observe a re-armed
> sample request after an invoke sees the CALLEE, at `pc=0 last_pc=0`, having
> executed nothing.** The invoke operation's own cost is therefore reported
> against the callee's *entry*. Aggregating leaf frames by method — which is
> what this page has done three times — files that cost under the callee's
> name, where it reads as "this body is slow".
>
> Calibrated, not argued. `probes/InvokeAttributionProbe.java` puts a
> three-bytecode `callee()` behind an `invokevirtual` in a loop, so nearly all
> of the loop's cost is invoke overhead **by construction**, and prints the
> per-iteration delta against the same loop with the call written out:
>
> | | |
> |---|---|
> | `withCall` | 388–512 ns/iteration |
> | `noCall` (control) | 94–124 ns/iteration |
> | **invoke delta** | **290–417 ns per interpreted invoke** |
> | samples at `callee` `pc=0 last_pc=0` | **37 of 69 = 53.6%** |
> | samples anywhere in `callee`'s body | 1 |
>
> A body-weighted profiler would put ~3/14 of that loop in `callee`. The entry
> bucket alone takes 54%, and it tracks the timed invoke share. Confirmed.
>
> ### What this page's profile actually says
>
> Re-taken 2026-08-11 on `dev` `08c8e1891`, Windows, `AnnotationScanCostProbe`
> over all 35 `output/build/lib` jars (38 s, `--stack-sample-ms 100`, 373 leaf
> samples), split by whether the frame had executed anything:
>
> | bucket | samples | share |
> |---|---:|---:|
> | **`pc=0 last_pc=0` — the invoke that pushed the frame** | **197** | **52.8%** |
> |   …of which `ConstantPool.getConstant(I,Class)` | 147 | 39.4% |
> |   …of which `BufferedInputStream.read` | 34 | 9.1% |
> | in-body, `BufferedInputStream.read1` | 83 | 22.3% |
> | in-body, `ConstantPool.getConstant` | 37 | 9.9% |
> | in-body, `ConstantPool.<init>` | 23 | 6.2% |
>
> **Over half of this workload's interpreted time is the invoke operation**,
> and one call-site family — BCEL's per-constant-pool-access
> `getConstant(int, Class)` — is 39% of it.
>
> That re-reads both profiles this page argued from:
>
> * § Handoff 2026-08-07's "**55.21% `ConstantPool.getConstant`**" is not
>   `getConstant`'s body. It is the cost of *invoking* it, 1.77 M times.
> * The 2026-08-06 update's "**78.3% of all interpreted time in five
>   `BufferedInputStream` bodies**" is the same shape, and its conclusion —
>   "none of them can compile … that is why every tier-up lever moved nothing"
>   — reached the right verdict for the wrong reason. Compiling those bodies
>   would not have helped, because the time is not in them.
>
> It also explains the negative result this page found most interesting: every
> lever that compiled or admitted a *callee* moved nothing, because the cost is
> **reaching** the callee. `CRATONVM_JIT=sync-methods`, `loop-work-tierup` and
> `special-tierup` were all aimed one frame too deep.
>
> What is left is the interpreted invoke path itself, at ~350 ns against
> HotSpot's interpreter at ~4 ns for the same operation. § The number that
> actually sizes this reached ~260–490 ns independently, and is the one row on
> this page that was already measuring the right thing. This is criterion 3's
> project, now with a profile that points straight at it and an 8-second A/B
> harness (`InvokeAttributionProbe`) to price candidate changes without paying
> for a 35-second scan.
>
> ### The 2026-08-11 `JarFile`-accessor fix does not move this page
>
> Recorded so it is not assumed.
> `fixed-bugs/jarfile-accessors-stat-the-file-on-every-call-FIXED-20260811.md`
> removed a `std::fs::metadata` (20–54 us on Windows) from every `JarFile`
> accessor call — worth 5–18x on a jar walk and −27% on
> `TomcatServletWebServerFactoryTests`. On this probe it is **inside the
> noise**. Four interleaved passes, arm order reversed on even passes, HotSpot
> control every pass, `taglibs-standard-impl`, us/class:
>
> | arm | p1 | p2 | p3 | p4 | mean |
> |---|---:|---:|---:|---:|---:|
> | before (`dev` `e05bbe374`) | 854.1 | 730.8 | 593.0 | 668.6 | **711.6** |
> | after (`dev` `08c8e1891`) | 876.3 | 680.0 | 524.6 | 850.8 | **732.9** |
> | HotSpot 25 | 6.7 | 12.6 | 17.0 | 19.9 | **14.1** |
>
> Total overlap in both orders. The reason is structural rather than
> surprising: the probe reports `parse` as `read+parse` minus `read`, and the
> per-entry `getInputStream` the fix speeds up is paid in **both** terms, so it
> cancels out of the headline. A real deploy cancels nothing, which is why the
> same fix is large there and absent here — one more reason not to use this
> probe as the profile of record (§ Methodological finding).
>
> ### Criterion 2, re-verified
>
> 711.6 / 732.9 us/class against the 813.5 us/class band set on 2026-08-06:
> **within band, no regression.** The cross-VM ratio reads ~50x here against
> the ~116x recorded on Azure, which is a host difference (this host's HotSpot
> column is 6.7–19.9 us/class) and not progress. Take the
> CratonVM-vs-CratonVM column, as § Measuring this at all already says.
>
> ### The ~350 ns invoke, decomposed under `perf` — with a control arm
>
> Azure `20.80.105.49`, `--nojit`, `perf record -F 997`, flat, load average 11
> (so read the shares, not any wall clock). `InvokeAttributionProbe` reproduces
> on Linux at **450–455 ns with the call, 94–101 ns without, delta 353–370 ns**,
> matching the Windows figure.
>
> The point of the probe's two arms is that the **`nocall` arm is a control**,
> and it is a remarkably clean one — three symbols and nothing else:
>
> | `nocall` (no invoke at all) | |
> |---|---:|
> | `execute_frame_from_index` | 77.40% |
> | `safepoint_check` | 18.17% |
> | `try_osr_with_backoff` | 3.07% |
>
> **So every other symbol in the `call` arm is the invoke path**, which is what
> makes the following a decomposition rather than a list:
>
> | `call` arm symbol | share | group |
> |---|---:|---|
> | `execute_frame_from_index` | 25.67% | *(loop — also in the control)* |
> | `execute_invokevirtual_cached` | 15.43% | dispatcher body |
> | `pop_and_recycle_frame_with_reason` | 6.97% | frame lifecycle |
> | `__memmove_avx512_unaligned_erms` | 6.43% | frame lifecycle |
> | `safepoint_check` | 4.33% | *(control)* |
> | `Frame::new_pooled_cached` | 4.25% | frame lifecycle |
> | `CachedInvokeTarget::clone` | 2.55% | cache |
> | `InvokeCache::get` | 2.46% | cache |
> | `VmHeap::is_object_address` | 2.28% | receiver checks |
> | `ZObjectStarts::contains` | 1.98% | receiver checks |
> | `init_locals_from_parts` | 1.98% | frame lifecycle |
> | `try_osr_with_backoff` | 1.70% | *(control)* |
> | `copy_args_to_locals` | 1.69% | frame lifecycle |
> | `CompactValue::decode_by_descriptor` | 1.65% | arg decode |
> | `execute_invokevirtual_cached::{closure#9}` | 1.65% | dispatcher body |
> | `OrderedPlRwLock<ClassManager>::try_read` / `::read` / guard drop | 1.53 / 1.14 / 0.99% | class-manager lock |
> | `real_http_url_connection_native` | 1.50% | native-interception chain |
> | `intercept_force_registered_native_cached` | 1.47% | native-interception chain |
> | `drop_glue<FrameInner>` | 1.29% | frame lifecycle |
> | `__memset_avx512_unaligned_erms` | 1.08% | frame lifecycle |
> | `ValueStack::from_pooled` | 0.99% | frame lifecycle |
> | `refresh_stale_object_args` | 0.92% | receiver checks |
> | `VmHeap::class_id_of` / `load_and_forward` | 0.72 / 0.68% | receiver checks |
> | `push_frame_and_fire_entry` | 0.72% | JVMTI |
> | `pop_arg_for_descriptor_checked` | 0.63% | arg decode |
>
> Grouped, as a share of the whole `call` arm:
>
> | group | share |
> |---|---:|
> | **frame lifecycle** (construct, fill locals, move in, move out, drop) | **~24.7%** |
> | dispatcher body (`execute_invokevirtual_cached` + its closure) | ~17.1% |
> | receiver / heap checks | ~6.6% |
> | inline-cache lookup + `CachedInvokeTarget::clone` | ~5.0% |
> | class-manager `RwLock` read, per invoke | ~3.7% |
> | native-interception chain | ~3.0% |
> | argument decode | ~2.3% |
>
> **The largest single item is not the dispatcher, it is the frame.** A
> call-graph run (`--call-graph=dwarf`) puts the `memcpy` under
> `pop_and_recycle_frame_with_reason` → `Vec::pop<Frame>` and under
> `push_frame_and_fire_entry`: `Frame` is a large by-value struct and it is
> **moved on every push and every pop**. That is the shape of the remaining
> gap, and it is a data-structure change to the interpreter's frame stack —
> the "genuine interpreter rewrite" this page has been calling for, now with a
> number on it.
>
> Two smaller items are ordinary defects rather than architecture, and are the
> only things here a point fix could reach:
>
> * **The native-interception chain, ~3.0%, is per-call-site constant.**
>   `real_http_url_connection_native` appearing at 1.50% in a probe whose only
>   call is `int callee(int)` is the tell: a `(class, method, descriptor)`
>   match chain runs on every inline-cache **hit**. § Correction: it is not
>   `try_stackless_invoke` already identified this as "where the precomputed
>   flags half of the project belongs" — it now has a price.
> * **A class-manager `RwLock` read per invoke, ~3.7%**, the invoke-side twin
>   of the field-path finding in § The clusters, by mechanism.
>
> ### First piece taken: the returning frame is recycled in place
>
> The return path opened with `if let Some(f) = thread.frames.pop()`, and `f`
> then travelled into `recycle_frame_with_shared` and again into
> `take_pool_parts` — **three moves of a ~300-byte `Frame` to arrive at four
> `Vec` headers**. Nothing on that path needed the frame anywhere but where it
> already was. It now reads the dying frame through a borrow, harvests the four
> buffers by header (`std::mem::take`), and lets `FrameStack::truncate` drop
> the husk where it lies.
>
> **The mechanism moved, and only the mechanism** — same probe, same host, both
> binaries, `perf` shares (load-independent, which matters: the box was at load
> 17):
>
> | symbol | before | after |
> |---|---:|---:|
> | `__memmove_avx512_unaligned_erms` | 5.75% | **2.64%** |
> | `pop_and_recycle_frame_with_reason` | 6.10% | **3.92%** |
> | `execute_invokevirtual_cached` | 15.21% | 14.82% |
> | `Frame::new_pooled_cached` | 3.84% | 3.87% |
> | `is_object_address` | 2.40% | 2.45% |
> | `CachedInvokeTarget::clone` | 2.33% | 2.36% |
> | `InvokeCache::get` | 2.12% | 2.17% |
> | `init_locals_from_parts` | 1.88% | 1.89% |
>
> **−5.3 percentage points of the invoke arm**, entirely in the two symbols the
> change targets; every other symbol is flat. A call-graph re-run confirms the
> `memcpy` under `Vec::pop<Frame>` is gone — what remains is attributed to
> `intercept_force_registered_native_cached`, i.e. the *other* item on the list
> above.
>
> **On the workload it is ~2%, and this host cannot resolve that.** Four
> interleaved passes, arms reversed on even passes, `taglibs-standard-impl`
> us/class: before 579.6 / 699.8 / 751.2 / 846.5 (mean **719.3**), after
> 673.7 / 703.2 / 724.7 / 717.3 (mean **704.7**), HotSpot 10.7–20.6. The means
> differ by 2.0% and the ranges overlap, so **the workload figure is a
> prediction from the mechanism, not a measurement** — which is what the
> arithmetic says to expect: −5.3 pp of an invoke arm that is about half the
> scan's interpreted time. The `after` column being much tighter (674–725
> against 580–847) is suggestive, and is not evidence.
>
> Correctness: 2487 `cratonvm-vm` unit tests and the 38-class regression suite
> green on the changed binary. `regression-suite/perf/c2-reach.sh` and a
> CratonBench pass were **not** run and are not implicated — this change alters
> no admission or tier-up decision, so nothing moves between the tiers those
> gates watch.
>
> **What is left of the frame group.** `Frame::new_pooled_cached` (3.87%),
> `init_locals_from_parts` (1.89%) and `copy_args_to_locals` (1.64%) are the
> push side, and the symmetric fix — constructing into the slot rather than
> moving into it — is **not** justified on this evidence:
> `push_frame_and_fire_entry` no longer appears among the `memcpy` callers at
> all after this change, so the push-side move is either already elided by the
> compiler or below 0.5%. Re-measure before building it.
>
> ### Second piece taken: the interception chain is classified once per call site
>
> `intercept_force_registered_native_cached` runs on every inline-cache hit.
> Below its memoized `force_native_cache` sat three arms still evaluated from
> scratch every time, and **every one of their keys is a function of the call
> site's own triple**:
>
> * a `ClassLoader` null-resource re-target — `(method_name, descriptor)`
>   against three pairs;
> * a `java/lang/Class` reflection re-target — the same pair against four more;
> * `real_http_url_connection_native`, whose *entire* gate is `class_name`
>   against five literals.
>
> `CachedBytecodeMethod` now carries an `intercept_shape_cache: OnceLock<u8>`
> classifying the triple against all three, once. The argument- and
> receiver-dependent halves are untouched: a set bit still runs the original
> test in full, and a clear bit skips a test whose name-keyed half could not
> have matched.
>
> | symbol | before | after |
> |---|---:|---:|
> | `intercept_force_registered_native_cached` | 1.44% | **0.99%** |
> | `real_http_url_connection_native` | 1.29% | **absent** |
> | **total** | **2.73%** | **0.99%** |
>
> **-1.74 percentage points, a 64% cut**, and one function leaves the hot path
> entirely. Wall clock, four interleaved passes at load 20: before mean 318 ns
> per invoke (303-344), after 300.5 (275-326) — the ranges overlap, so as with
> the frame change the mechanism is the evidence and the wall clock is not.
> 2490 unit tests and the 38-class regression suite green.
>
> **A recorded negative, because it is the interesting half.** The first
> version added a `shape == 0` early return into a shared tail function, on the
> reasoning that the common call site should not even step over three bit
> tests. Measured, that was **worse than leaving the control flow alone**:
> entry 1.05% + tail 1.12% = 2.17%, against 0.99% for the same string-work
> removal with the arms guarded in place and no split. The function boundary
> cost more than the three bit tests it skipped. The comment in
> `intercept_force_registered_native_cached` says so, so the shortcut does not
> get reinvented.
>
> ### Third piece: half the per-invoke class-manager lock, and a working instrument
>
> **The instrument first.** `--call-graph=dwarf` could not attribute this: it
> named two inlined callers, `intercept_classloader_set_default_assertion_status`
> and `init_locals_from_parts`, and **neither takes a lock** (checked against
> the source). `--no-inline` collapsed the chains to the symbol itself with one
> arm at a bare `0x18700000000` — the unwinder had no usable parents at all.
> A rebuild with `RUSTFLAGS="-C force-frame-pointers=yes"` and
> `perf record --call-graph=fp` named the caller immediately and correctly.
> **Use a frame-pointer build for any call-graph question on this binary.**
>
> It put both acquisitions directly in `execute_invokevirtual_cached`:
> `try_read` 1.85%, `read` 1.43%, read-guard `drop_glue` 1.28%.
>
> **What `try_read` was.** The virtual tier-up gate computed two predicates
> into `let` bindings *above* the `if` that consumes them:
> `has_registered_native` (a `NativeMethodRegistry` resolve) and
> `receiver_is_java_util` (class-manager `try_read` + `get_class` +
> `starts_with("java/util/")`). The `&&` chain below them is ordered cheapest-
> first and short-circuits — but eager `let`s never see it. Under `--nojit`,
> where `!disable_jit()` makes the chain fail several conditions earlier, the
> work was done anyway, on **every cached invoke in the VM**, to decide an
> optional tier-up that could not happen.
>
> Both are now closures called in place in the chain, and
> `has_registered_native()` is ordered after the JIT kill-switch. Every
> condition here is a pure predicate, so `&&` may order them freely.
>
> `try_read` **disappears from the profile entirely**. And with the host
> finally quiet (load 3.5), six interleaved passes, arm order reversed each
> pass, ns per interpreted invoke:
>
> | | p1 | p2 | p3 | p4 | p5 | p6 | mean |
> |---|---:|---:|---:|---:|---:|---:|---:|
> | before | 203 | 202 | 201 | 201 | 201 | 202 | **201.7** |
> | after | 193 | 195 | 194 | 193 | 194 | 195 | **194.0** |
>
> **-3.8%, 6/6, and no overlap between the two columns** — the first fully
> separated wall-clock reading in this whole sequence, which is what a quiet
> host buys and nothing else does. 2490 unit tests and the 38-class regression
> suite green.
>
> **The other half is now attributed, not fixed.** The remaining `::read`
> (2.01%, same function) is `dispatch_virtual.rs`'s annotation-proxy gate: on
> every non-`invokespecial` virtual invoke it takes the class-manager read
> lock, calls `get_class(actual_class_id)` and compares the name against the
> single literal `"java/lang/annotation/AnnotationProxy"`. Unlike the tier-up
> predicates it is a **correctness** gate consumed immediately, so it cannot be
> deferred — it has to become an identity test. Resolve that one class's
> `ClassId` once and compare ids; a name comparison per invoke is also exactly
> the shape `reference_class_name_shape_tests_are_dispatch_bugs` warns about.
> It needs generation-aware memoization (a class defined later must not be
> missed), which is why it is recorded here rather than guessed at.
>
> ### The annotation-proxy gate: scoped, and why it is a cache-population change
>
> The last named item, ~2.0% of the invoke arm. On every non-`invokespecial`
> virtual invoke `execute_invokevirtual_cached` takes the class-manager read
> lock, calls `get_class(actual_class_id)` and compares the name against one
> literal, `"java/lang/annotation/AnnotationProxy"`, to decide whether to force
> a `CacheMiss`.
>
> Two things were checked before proposing anything, and both change the answer:
>
> * **It cannot be memoized on `CachedBytecodeMethod`**, which is where the
>   other two per-call-site memos on this path live
>   (`force_native_cache`, `intercept_shape_cache`). That struct describes the
>   resolved *target method*, whose declaring class is frequently a supertype —
>   an `AnnotationProxy` receiver calling an inherited `Object` method shares
>   its entry with every other receiver of that method. A bit cached there
>   would answer for the wrong class.
> * **It cannot be deferred** the way the tier-up predicates were. Those gate an
>   optional promotion; this one is consumed immediately and decides
>   correctness.
>
> What makes it tractable is the branch above it: when
> `actual_class_id != receiver_class_id` the code either rebinds to the
> polymorphic entry **for `actual_class_id`** or returns `CacheMiss`. So by the
> time the gate runs, the live `CachedInvokeTarget::VirtualBytecode` is the
> entry for exactly this receiver class — and "is this receiver class the
> annotation proxy" is a **per-cache-entry constant**.
>
> **So the fix is to compute it once at cache-population time**
> (`populate_virtual_invoke_cache` already holds the class manager) and store a
> bool on the `VirtualBytecode` variant, leaving the hit path a field test.
> Entry invalidation is already handled by `entry_gate.generation`, so this
> needs no epoch key of its own — unlike the alternative of a global
> `ClassId`-keyed memo, which would have to answer two questions this
> investigation has not: whether that name can be defined under more than one
> loader, and whether a `ClassId` can be recycled after class unloading
> (`RClassUnloadSweep` says unloading exists). Guessing either one wrong in a
> correctness gate is the failure mode this page already documents five times.
>
> It touches `CachedInvokeTarget` — a hot enum cloned on every cache hit — and
> every site that constructs the variant, which is why it is scoped here rather
> than done alongside the three smaller fixes above. `class_definition_epoch()`
> (one `Acquire` load) is the right key if a global memo is chosen instead.
>
> **Coordinate first**: `fix/jdk-only-strict-annotation-proxy-20260811` was an
> active worktree while this was written and is likely editing the same
> predicate for policy reasons.
>
> **One caution about that call-graph run**, because it nearly cost a session:
> `perf` also attributed a 3.16% `memcpy` arm to `dbg_loader_trace` inlined
> inside `execute_invokevirtual_cached`, which would have been a spectacular
> find — a debug predicate copying memory on every invoke. It is not real.
> `dbg_loader_trace()` is `cached_is_ok!`, a memoised `MemoSlot` load that
> cannot copy anything. `--call-graph=dwarf` mis-nests inlined frames, so an
> inline attribution has to be checked against the source before it is
> believed; the two non-inlined attributions in the same output
> (`Vec::pop<Frame>`, `push_frame_and_fire_entry`) are the trustworthy ones.

> **Update 2026-08-07 — I tried to close this and could not. Here is the
> measured ceiling, three corrections to what is written below, and a re-scope.**
>
> Everything here is on `origin/dev` `e1b6c99a3` (the tip after the 2026-08-06
> fixes), measured on the **real deploy** — `TestManagerWebapp.testBug57700`
> driven alone — because § the 2026-08-06 update established that this doc's
> probe is not a safe profile of record.
>
> ### Correction 1 — `--nojit` is now 35% SLOWER, not faster
>
> § What this is NOT — measured calls it "the decisive measurement": *"on a
> quiet host `--nojit` is faster than the default … Compiled code contributes
> nothing to this workload"*, and § Exit criteria builds "**not reachable by
> tiering work**" on top of it. **That is stale.** Deploy of `/bug57700`,
> interleaved, two runs each:
>
> | arm | deploy |
> |---|---|
> | default | 15 886 / 15 063 ms |
> | `--nojit` | 21 884 / 20 131 ms |
>
> The JIT is now worth **~26%**. The regime changed when the per-two-byte
> native re-entry was removed: that work was *unreachable* by the compiler (a
> Rust native calling interpreted `BufferedInputStream.read`), so compilation
> genuinely had nothing to bite on. What is left is BCEL's own Java, which the
> compiler can and does compile. **Tiering work is back on the table**; the
> paragraph below saying otherwise should not be planned against.
>
> ### Correction 2 — the native-call funnel is not the wall either (10%)
>
> Priced properly this time, not estimated. `--dump-native-registry` over one
> deploy: **12 689 143 native invocations**, led by
>
> | native | calls |
> |---|---|
> | `DataInputStream.readByte()B` | 3 161 278 |
> | `Objects.requireNonNull(Object,String)` | 1 733 844 |
> | `DataInputStream.readUTF()` | 1 733 792 |
> | `DataInputStream.skipBytes(I)I` | 1 630 912 |
> | `DataInputStream.readUnsignedShort()I` | 1 200 604 |
> | `Class.isAssignableFrom` / `Class.cast` | 442 994 / 442 836 |
>
> (the last two are one pair per constant-pool access — BCEL's
> `ConstantPool.getConstant(int, Class)` does `isAssignableFrom` then `cast`.)
>
> The two in-tree profilers price a call end to end
> (`cargo test --release -p cratonvm-vm --lib -- --ignored --nocapture
> funnel_cost_breakdown` and `jit_native_dispatch`):
>
> | step | ns |
> |---|---|
> | `safe_native_call` body, 1 object arg | 48 |
> | ... of which `2x record_transition` | 15 |
> | ... of which `load_and_forward` | 10 |
> | `is_object_address(receiver)` | 15 |
> | `class_id_of(receiver)` (vs `class_id_of_validated` at 1.1) | 12 |
> | `forward_jit_reference_args`, 1 recv | 12 |
> | `note_site_identity()` warm hit | 11 |
> | `decode_dispatch_values`, 1 recv (vs `decode_..._into` resolved at 5.2) | 16 |
>
> ≈ **120 ns** for a compiled-code native call. 12.69 M × 120 ns ≈ **1.5 s of a
> 15.5 s deploy — 10%.** Driving it to zero leaves ~15x against HotSpot, not 5x.
>
> ### Correction 3 — by-name field resolution, falsified as a lever
>
> `get_field_by_name` / `set_field_by_name` take the class-manager `RwLock` and
> walk the superclass chain comparing field *names* on **every** call, and the
> buffered `DataInputStream` fast path reads `buf`/`pos`/`count` and writes
> `pos` per typed read — ~32 M walks per deploy. A thread-local, epoch-validated
> `(ClassId, name) -> slot` memo modelled exactly on `FIELD_DESCRIPTOR_RING`
> (per-entry epoch, direct-mapped, name compared inline on every hit, 31/31
> regression green) measured **1.9% over ten interleaved runs per arm** —
> base 15 703–18 296, memo 15 229–16 881. Inside the noise. **Reverted, not
> shipped**, on this investigation's own standing rule about levers that do not
> measure. The `with_class_layout` / `VersionCache::find` / `__memcmp` share I
> had attributed to name resolution belongs to the *descriptor* path that
> ordinary `get_field`/`set_field` use, not to this.
>
> ### Where the time is now
>
> Genuinely flat. `perf record` over the deploy, symbols above 1%:
> `is_object_address` **7.0**, `__memcmp` 3.1, `_mi_page_malloc_zero` 2.6,
> `execute_frame_from_index` 2.4, `with_class_layout` 2.2,
> `safe_native_call_impl` 1.9, `single_thread_guard_enabled` 1.6,
> `get_field_by_name` 1.6, `record_object_ref_payload_slow` 1.5,
> `VersionCache::find` 1.3, `jit_invoke_dispatch` 1.3, `execute_instruction` 1.2,
> `try_jit_site_cached_native_dispatch` 1.2, `forward_jit_reference_args` 1.1,
> `pin_jit_code_range_owner` 1.1, `invoke_on_class_shared_inner` 1.1,
> `slot_for_exact` 1.0. Nothing above 7%; the listed symbols total ~35%.
>
> ### The one lever that is left, and why it is not "count more"
>
> `CRATONVM_DBG=jit-method-stats` sees **271 methods and 133 534 invocations**.
> `CRATONVM_DBG=invokestats` sees **22 400 001 inline-cache hits** in the same
> run. **99.4% of invokes never reach the tier-up counter** — and only 3 methods
> are `hot_but_stuck_in_interpreter`, at 1716 / 948 / 500 invocations, so the
> manager does not even know it is blind.
>
> The exclusions are all in `execute_invokevirtual_cached`'s tier-up block:
> `!is_special`, `!cached.is_synchronized`, `!has_registered_native`,
> `!receiver_is_java_util`, `cached.exception_table.is_empty()`. BCEL's parse is
> full of every one of those.
>
> **Do not "just count them".** That was built and measured last session
> (`CRATONVM_JIT=special-tierup`, reverted): counted invocations moved
> 129 391 → 129 455, the compiled census 672 → 674, and wall-clock got *worse*.
> Compiling a method whose only consumer is the very call site that excluded it
> buys nothing. The lever is making the **direct compiled call legal** for
> handler-bearing and `java.util` callees — i.e. exception resumption across a
> direct compiled call, and the stale receiver-specific entry that
> `receiver_is_java_util` was added to avoid. That is the project this doc has
> been describing all along; it is now bounded by measurement rather than
> asserted from a probe profile.
>
> ### Re-scoped exit criteria
>
> The original criterion — `AnnotationScanCostProbe` within ~5x of HotSpot —
> required **~16x** from here. Every item identified and priced above sums to
> well under 2x (native calls 1.10x, field resolution 1.02x, and a 1–7% tail
> with no member above 7%). It is not reachable by point fixes, and this doc
> already offered the alternative: *"either attack interpreter dispatch cost
> broadly, or re-scope the exit criteria."* This is the re-scope.
>
> 1. **No test fails on deploy timing. — MET 2026-08-06.** `TestManagerWebapp`
>    is `OK (3 tests)`; the two methods this doc names pass; 15 further
>    deploy/parse-heavy Tomcat classes were differentially checked against
>    pristine `dev` with identical outcomes. This is the criterion that made the
>    doc `Severity: high`, and it is satisfied.
> 2. **Deploy throughput stays within its measured band. — OPEN, tracking only.**
>    `testBug57700`'s deploy is ~15.5 s against HotSpot's ~0.96 s (~16x) and
>    `AnnotationScanCostProbe` ~815 µs/class. Treat a regression past those as a
>    bug; do not treat the gap itself as one.
> 3. **The gap closes with the direct-compiled-call project, not here. — the
>    real work.** Owner should be a JIT/interpreter-dispatch item with
>    `regression-suite/perf/c2-reach.sh` plus a CratonBench pass in scope, as
>    § Untaken levers already says. Setting a throughput number before that
>    project scopes itself would be inventing one.
>
>    **2026-08-11: that project now has a measured target.** Over half of this
>    workload's interpreted time is the invoke operation, ~350 ns of it, and
>    the biggest piece is not dispatch logic but **moving a by-value `Frame`
>    in and out of the frame stack** (~24.7% of the invoke arm). See the
>    2026-08-11 update at the top for the control-arm decomposition.
>
> `probes/NativeBridgeCostProbe.java` (added with this update) is the tool for
> the recurring "is this bridge worth it" question: it prices a registered
> native against a byte-for-byte equivalent body that has no registration, in
> the same process. Today, on CratonVM: `Objects.requireNonNull` 577 ns bridged
> vs 240 ns as bytecode; `Class.isAssignableFrom`+`cast` 1478 ns vs 12 ns. On
> HotSpot both columns are 1–5 ns and the ratio is ~1. A bridge over a
> five-bytecode body is a pessimization here, and there are 1.73 M
> `requireNonNull` calls in one deploy.

> **Update 2026-08-06 — 4.04x, and § What this is NOT — measured was wrong
> about the mechanism. Both named test methods now PASS.**
>
> This doc's conclusion — "it is not a gate at all, it is interpreter
> throughput", "the `perf` profile is correspondingly flat … no hotspot to
> remove", "not reachable by tiering work … a genuine interpreter rewrite" —
> was **premature**. Profiling the **real webapp deploy** (rather than
> `AnnotationScanCostProbe`) found three removable costs worth 4.04x on this
> doc's own metric and 4.56x on the deploy the metric stands in for. Full
> write-up, evidence and commits:
> `fixed-suite-bugs/tomcat/testmanagerwebapp-post-seek0fix-read-timeout-FIXED.md`.
>
> * **`jmx_locked_monitors` leaked, and the leak was quadratic — 14.9% of the
>   run.** Two of the four `complete_jmx_monitor_enter` publishers never
>   retracted, so the per-thread owned-monitor set grew without bound and the
>   linear dedupe scan every `monitorenter` runs grew with it. `perf annotate`
>   put 96% of that 14.9% inside the scan loop. Commit `62d28988e`.
>   **This doc dismissed exactly this symbol**, on the strength of the probe's
>   profile: "the only monitor symbol that appears at all is
>   `complete_jmx_monitor_enter`, at 1.47%". On the real deploy it is 14.87%.
> * **`Multi-Release` was re-parsed once per jar ENTRY — 8.3%.** A whole-manifest
>   `from_utf8_lossy` per lookup, memoized per (jar, mtime). Commit `3517a77fe`.
>   Invisible to the probe, which opens one jar where the deploy opens hundreds.
> * **Typed `DataInputStream` reads re-entered the interpreter per two bytes —
>   the actual mechanism of this doc's headline number.** `dis_read_exact`
>   allocated a Java `byte[2]` and ran the interpreted
>   `BufferedInputStream.read(byte[],int,int)` chain for every
>   `readUnsignedShort`. Now it copies from the stream's own buffer in Rust.
>   Commit `e899910c2`.
>
> **The third one supersedes this doc's root-cause section outright**, and it
> also explains the tier-up mystery this doc kept circling. `--stack-sample-ms`
> over the real deploy puts **78.3% of all interpreted time** in five
> `BufferedInputStream` bodies — `read` 30.6, `read1` 23.0, `getBufIfOpen` 12.9,
> `ensureOpen` 6.2, `fill` 5.6 — and **none of them can compile**: `read` and
> `read(byte[],int,int)` are `ACC_SYNCHRONIZED`, and the other four are private
> and reached **only from a native re-entry**, so they have no inline-cache site
> and the tiering manager never counts them at all. `jit-method-stats` sees
> 129 k counted invocations against `invokestats`' 22.4 M cache hits.
>
> That is why every tier-up lever this doc tried moved nothing: the code that
> mattered was never on a counted path. `CRATONVM_JIT=special-tierup` — the
> "invokespecial stops feeding the tiered manager once cached" lever recorded
> under § Untaken levers — was built and measured for this update: counted
> invocations 129 391 → 129 455, compiled census 672 → 674, no
> `BufferedInputStream` body compiled, and *worse* wall-clock combined with
> `sync-methods`. It was reverted; do not rebuild it on that rationale.
>
> **New numbers**, Azure host, arms interleaved ABBA, pristine arm = `origin/dev`
> `c3919f7d3` built from a detached worktree:
>
> | measurement | before | after | HotSpot |
> |---|---|---|---|
> | `AnnotationScanCostProbe` (`taglibs-standard-impl`) | **3284.5** µs/class | **813.5** µs/class | ~5–19 (≈2 ms total; noise at this scale) |
> | `testBug57700` deploy of `/bug57700` | **74 323** ms, 4/4 FAIL | **16 311** ms, 4/4 PASS | 956 ms |
> | `TestManagerWebapp` whole class | 84 s, `Failures: 1` | 50–58 s, **`OK (3 tests)`** | 8.9 s |
>
> Probe readings, no overlap in either order: before 3191.0–3482.9, after
> 726.2–879.5.
>
> **Both test methods in this doc's § Symptom now pass**, so this doc's
> remaining scope is throughput, not a failing test. The exit criterion (~5x of
> HotSpot) is still not met and the doc stays OPEN — but the standing advice
> above ("either attack interpreter dispatch cost broadly, or re-scope the exit
> criteria") should be read with the caveat that it was written from a
> profile of the wrong workload. After these three fixes the deploy's hottest
> Rust symbol is `is_object_address` at **6.0%**, and the profile below it is
> genuinely flat, so *that* claim now rests on a measurement of the real thing.
>
> **Methodological finding, and the reason this took four sessions:**
> `AnnotationScanCostProbe` is **not representative of the deploy it stands in
> for**. It under-reported the monitor cost 10x and could not see the jar
> manifest cost at all. It remains a good A/B lever — it is stable and it moves
> with the real thing — but it must not be used as the *profile* of record. Take
> that from `perf record` on the actual failing test method, run alone on a
> quiet host.

> **Update 2026-08-03 — two corrections, neither of which closes this doc.**
>
> 1. **The compiled-method census below is stale.** Re-run on merged `dev`
>    `5e1f7d6e3` over the same `AnnotationScanCostProbe` scan,
>    `CRATONVM_DBG=jit-compiled` lists **21 methods, two of them `<init>`**
>    (`ConstantUtf8.<init>`, `ConstantClass.<init>`), not "eight, zero
>    `<init>`". `Constant.readConstant`, `ConstantUtf8.getInstance` and
>    `Utility.getClassName` compile now too. So **"the parse is never compiled"
>    and "zero constructors" are both out of date**, and § Root cause — which
>    argues from them — must be re-derived before it is planned against. What
>    is still absent is exactly the frame `--stack-dump-on-timeout` puts on
>    top: `BufferedInputStream.read`, `DataInputStream.readUnsignedByte`,
>    `ClassParser.parse`, `ConstantPool.<init>` — i.e. the `synchronized` /
>    lock-bearing bodies, which is doc 31's subject, not an admission ban.
> 2. **A separate degradation term was found and fixed**, and it is not in this
>    doc's model at all: the cost is not flat, it *rises* within one process.
>    See `fixed-suite-bugs/tomcat/loader-latch-degrades-every-deploy-FIXED.md`.
>    Defining one class through any user-defined loader used to make the whole
>    VM ~1.8x slower permanently. Fixed; worth 1.6x on the probe and **nothing
>    measurable on the test classes**, which is why this doc stays OPEN.

> **Update 2026-08-04 — the "still absent" list above is itself half stale.**
>
> Two of the four frames now compile. `CRATONVM_DBG=jit-compiled` over
> `probes/LoaderStepOneShotProbe.java` (`lib all 6`) lists **27 methods, 3 of
> them constructors**, and includes `ClassParser.parse`; over
> `probes/LoaderStepCostProbe.java` it lists **31**, adding
> `ConstantPool.<init>`, `Constant.<init>` and `JavaClass.<init>`. So of the
> four, only the **JDK's own two** — `BufferedInputStream.read` and
> `DataInputStream.readUnsignedByte` — are reliably never compiled, and those
> are exactly the `synchronized` bodies doc 31 owns. Anything arguing "the
> parse never compiles" is arguing from a census that no longer holds; the
> binding constraint is the per-class *flat* cost (~3 300–3 500 µs against
> HotSpot's ~16 µs), not compiled-vs-interpreted coverage of the BCEL classes.
>
> The loader-latch doc retired the same day, so the degradation term in
> correction 2 is now closed on measurement rather than on a wall-clock
> estimate: a positive-control build with the fix reverted steps **1.60x** at
> the first class definition and the shipped build is flat, and the "~1.5x
> second-loader step" that doc carried as an open residual **does not exist**
> and has been withdrawn.

## Untaken levers

Recorded here rather than lost when the loader-latch doc retired. Neither is
the binding constraint on this doc's classes; both are structurally real.

* **`invokespecial` call sites stop feeding the tiered manager once cached.**
  `vm/src/runtime/interpreter/dispatch_virtual.rs`'s invocation-counter block
  opens with `if !is_special`, and it gates *counting* as well as promotion —
  so once a constructor / private / `super` call site is in the inline cache it
  no longer increments the JIT invocation counter. The uncached route in
  `vm/src/runtime/interpreter.rs` still counts every method regardless of
  opcode, which is why the census above finds constructors compiled anyway, so
  the obvious framing ("constructors are invisible to tier-up") is **not**
  what the code does. Splitting counting from promotion is the small change;
  what makes it a project rather than a fix is the blast radius — widening
  which methods reach the optimizing tier moves work off the single-pass
  backend, whose loop lowerings have no IR-tier equivalent, so it needs
  `regression-suite/perf/c2-reach.sh` plus a CratonBench pass before it can
  land.
* **`new` has no per-call-site class-resolution cache.**
  `opcodes.rs`'s `Instruction::New` re-resolves its constant-pool entry on
  every execution, unlike `put_field`/`put_method`. This is what amplified the
  loader latch into a VM-wide 1.6x; with the latch fixed it is no longer a
  step, but it is still a per-`new` cost that a constant-pool parse pays once
  per entry.

## Symptom

`org.apache.catalina.manager.TestManagerWebapp` fails 2 of its 3 methods on
timing alone:

* `testBug57700` — `SocketTimeoutException: Read timed out` at
  `TestManagerWebapp.java:571`. The `GET /manager/text/deploy` it is waiting on
  runs longer than the client's 30 s read timeout. The deploy itself completes,
  just far too late: `HostConfig` logs `Deployment of web application directory
  [.../bug57700] has finished in [139,999] ms` against HotSpot's `[2 113] ms`.
* `testDeploy` — bare `assertTrue` at `TestManagerWebapp.java:436`, which
  asserts `/manager/text/list` contains `/examples:running`. The preceding
  reload is still in flight when `list` is served: CratonVM redeploys
  `examples` in `[13,312] ms` against HotSpot's `[402] ms`.

`testServlets` passes. Nothing is wrong with the deployed webapp — every deploy
completes correctly, it just misses the test's clock.

## Where the time goes

`--stack-dump-on-timeout=75` over the failing run, 8248 samples of the
serving thread:

| frame | samples | share |
|---|---|---|
| `java/io/BufferedInputStream.read([BII)I` | 7780 | 94.3% |
| `tomcat/util/bcel/classfile/ConstantPool.getConstant` | 388 | 4.7% |
| `tomcat/util/bcel/classfile/AnnotationEntry.<init>` | 36 | 0.4% |
| everything else | 44 | 0.5% |

all under
`ContextConfig.processAnnotationsJar -> processAnnotationsStream`, i.e. Tomcat
scanning every `.class` in the webapp's JARs for annotations.

`probes/AnnotationScanCostProbe.java` replicates that loop standalone — walk
each `.class` entry of a JAR and run Tomcat's own
`new ClassParser(is).parse()` — and separates reading from parsing:

| | HotSpot | CratonVM | ratio |
|---|---|---|---|
| read the entries (`taglibs-standard-impl`, 130 classes) | 3.1 ms | 11.3 ms | 3.6x |
| **BCEL-parse them** | **2.1 ms** | **481.7 ms** | **226x** |
| per class | 16.4 µs | 3705 µs | 226x |

**Re-verified on merged dev `36df168e4`** (29 commits later, including the x64
backend split), three interleaved rounds per VM back-to-back on the same host
state, `taglibs-standard-impl` parse only:

| round | HotSpot | CratonVM |
|---|---|---|
| 1 | 21.7 µs/class | 5440.1 µs/class |
| 2 | 11.7 µs/class | 5077.3 µs/class |
| 3 | 23.0 µs/class | 4959.6 µs/class |

Median-to-median **234x**. Take the ratio, not the absolute microseconds: this
is a shared, variably-loaded host, and HotSpot's own column swings 2x across
the three rounds because the whole parse is only ~2 ms there.

So the JAR/zip/inflate path is fine (`probes/JarEntryReadCostProbe.java`: 30.5
vs 62.7 MiB/s entry reads, raw `Inflater` 938 vs 1254 MiB/s — both under 2x).
The cost is the per-byte class-file parse.

> **Update 2026-08-04 (later) — three candidate root causes measured and
> FALSIFIED, and the section below is wrong about the mechanism.** See
> § What this is NOT — measured, which supersedes both this section and
> § Root cause. Short version: the cost is **general interpreter throughput**,
> not any single gate, and no tier-up lever moves it. `--nojit` is now measured
> *faster* than the default on a quiet host, so compiled code contributes
> nothing here at all.

## Where the cost is, decomposed (2026-08-04)

> This section supersedes § Root cause below on the question of *what* is slow.
> That section's shape — "this code never got compiled" — survives, but it
> names the wrong code, and the difference decides which fix is worth building.
>
> ⚠️ **The per-byte model below does not describe the real parse.** These
> stages are *synthetic* per-byte loops written by the probe; `ClassParser`
> itself reads in BULK. Counted with `--dump-native-registry` over the real
> scan (156 classes): 12,643 `DataInputStream.readUnsignedShort`, 4,768
> `readInt`, 938 bulk `ByteArrayInputStream.read([BII)` — and ~35k native calls
> in total, which cannot account for ~400 ms. `readUnsignedByte` does not even
> reach the top of the list. Do not plan against "~950 ns per byte".

`probes/AnnotationScanSplitProbe.java` runs five stages over the **same
in-memory class bytes**, each loop inlined into a named static method (never a
lambda — see the harness note in that file). 156 classes, 328 KiB,
steady-state round, ns/byte:

| stage | what it does | HotSpot | CratonVM |
|---|---|---|---|
| `parseMem` | `ClassParser.parse()` — construction + I/O chain | 4.8 | **982** |
| `readBytes` | `DataInputStream.readUnsignedByte()` per byte, **constructing nothing** | 0.3 | **971** |
| `readRaw` | `ByteArrayInputStream.read()` per byte — one layer less | 0.5 | **376** |

**`readBytes` alone is ~99% of `parseMem`.** Reading the bytes one at a time,
allocating nothing and parsing nothing, costs essentially the whole scan. So:

* it is **not** object construction, and not the constructor-compilation story
  the § below builds on;
* it is **not** class resolution or `new` — a per-call-site class-resolution
  cache was scoped and then dropped on the strength of this measurement;
* it is **not** the jar/inflate layer — `parseMem` (from a `byte[]`) matches
  `parse` (from the jar entry stream) to within noise.

It is the **per-byte I/O call chain**: ~376 ns for one
`ByteArrayInputStream.read()` (a one-line `synchronized` method) and ~600 ns
more for the `DataInputStream.readUnsignedByte()` wrapper, against HotSpot's
~0.5 ns. That is exactly the frame the stack dump named all along, and it puts
this doc in
`fixed-suite-bugs/tomcat/31-synchronized-code-never-jit-compiled-FIXED.md`'s
territory plus the VM-wide per-call floor — not in admission-gate territory.

The same probe used to **SIGSEGV on CratonVM** in its `arrayRead` stage on the
real Tomcat classpath — a separate, deterministic JIT miscompile, unrelated to
the throughput question this doc is about. Fixed 2026-08-04 (`14a274085`): a
slot javac reuses as both a live reference and a `long`'s high half must keep
its OSR register home. `probes/OsrRefSlotReuseProbe.java` is the regression
guard; the retired `annotation-scan-arrayread-sigsegv` write-up has the
analysis.

## Root cause: the parse is never compiled

`--nojit` costs the **same** as the default — on both the original build and
merged dev `36df168e4`:

| | JIT on | `--nojit` |
|---|---|---|
| `taglibs-standard-impl` parse (first measurement) | 444.9 ms | 437.8 ms |
| `taglibs-standard-spec` parse (first measurement) | 87.0 ms | 92.1 ms |
| `taglibs-standard-impl` parse (merged `36df168e4`) | 702.7 ms | 708.3 ms |
| `taglibs-standard-spec` parse (merged `36df168e4`) | 181.8 ms | 155.1 ms |

`CRATONVM_DBG=jit-compiled` over the whole scan (468 class parses) lists
**eight** compiled methods in total:

```
java/io/BufferedInputStream.close()V
java/io/FilterInputStream.read([B)I
org/apache/tomcat/util/bcel/classfile/ConstantClass.getTag()B
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(I)…
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(IB)…
org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant(ILjava/lang/Class;)…
org/apache/tomcat/util/bcel/classfile/ConstantUtf8.getTag()B
org/apache/tomcat/util/bcel/classfile/Utility.compactClassName(…)
```

Byte-for-byte the same list on merged dev `36df168e4`, still with a
`grep -c '<init>'` of **0**.

**Zero `<init>` methods** — in a workload whose whole shape is "construct one
object per constant-pool entry". `ClassParser.parse`, `ConstantPool.<init>`,
`Constant.readConstant`, `ConstantUtf8.<init>`, `AnnotationEntry.<init>`,
`DataInputStream.readUnsignedByte` and `BufferedInputStream.read()` are all
absent, and `CRATONVM_DBG=jit-method-stats` reports only 6 distinct methods
tracked with `hot_but_stuck_in_interpreter=0` — the tiering manager never sees
them at all, so they do not even register as stuck.

`probes/SingleByteReadCostProbe.java` prices the layers the parser sits on
(loops in named static methods, called directly — see the harness note in that
file, it matters). On merged dev `36df168e4`:

| operation | HotSpot | CratonVM |
|---|---|---|
| static call returning a field | 0.0 ns | 594.8 ns |
| `ReentrantLock` lock/unlock, uncontended | 15.2 ns | 18189.5 ns |
| `synchronized` enter/exit, uncontended | 4.8 ns | 1365.0 ns |
| `ByteArrayInputStream.read()` | 0.3 ns | 972.0 ns |
| `BufferedInputStream.read()` | 18.8 ns | 5424.1 ns |
| `DataInputStream.readUnsignedByte` over `BufferedInputStream` | 19.9 ns | 16412.7 ns |

> ⚠️ **Read this table for the ratios BETWEEN its own rows, not as absolute
> per-op costs, and do not quote its "static call returning a field" row as this
> VM's call floor.** `probes/CallFloorProbe.java` on the *same binary, same
> session* prices compiled call sites at **4.3 ns** (arith, no call), **11.9 ns**
> (invokestatic leaf), 42.5 ns (invokevirtual), 49.3 ns (invokeinterface) — so
> the 594.8 ns here is ~50x what the same operation costs in a probe that is
> definitely running compiled.
>
> Both probes' loops *are* compiled: `CRATONVM_DBG=jit-compiled,osr` shows
> `SingleByteReadCostProbe.floorLoop()J` OSR-compiled and `floor()I` compiled.
> But the OSR trace shows it **re-entering repeatedly** — at i=2000, 3000, 5000,
> 9000, 17000, the per-pc exponential back-off running to its 5-attempt cap —
> which means the compiled body keeps falling back to the interpreter. That is
> an unexplained second effect, plausibly the same family as this doc's, and it
> is why these absolutes are not trustworthy. `AnnotationScanCostProbe` is this
> doc's load-bearing measurement precisely because it times Tomcat's own code
> with no harness loop of ours in the middle.

The `CallFloorProbe` contrast is the useful part: **when this VM compiles a
method it is within a few x of HotSpot.** The 234x is "this code never got
compiled", not "the compiler emits bad code".

## What this is NOT — measured (2026-08-04, branch `perf/annotation-scan-monitor-wall-20260804`)

Everything in this section was measured on a **quiet host** (load ≈ 6–7; see
§ Measuring this at all) against the real `AnnotationScanCostProbe`, not a
microbenchmark. Baseline for all rows: **HotSpot ≈ 8 µs/class, CratonVM
≈ 1950 µs/class ⇒ ≈ 240x**, which reproduces this doc's 226–234x exactly.

**Not the monitor / `synchronized` cost.** Microbenchmarks are seductive here:
`SingleByteReadCostProbe` prices an uncontended `synchronized` round trip at
605.9 ns against HotSpot's 2.6 ns — 233x, temptingly equal to the headline
ratio. It is a coincidence. In the real scan's profile the only monitor symbol
that appears at all is `complete_jmx_monitor_enter`, at 1.47%.

**Not the `ACC_SYNCHRONIZED` JIT-admission gate.** `ByteArrayInputStream.read()`
genuinely never compiles — `jit_bridge.rs` rejected every `ACC_SYNCHRONIZED`
method on the invocation-counter path, *before* it was ever counted, which is
why `jit-method-stats` reported it neither compiled nor
`hot_but_stuck_in_interpreter`. Admitting them (`CRATONVM_JIT=sync-methods`,
default-off) changes this workload by **nothing**: 2237/2064 → 2140/2096
µs/class. The gap is real and worth closing on its own merits; it is not this.

**Not the outer loops failing to tier up** — though that gap is real too, and
is the most interesting negative result here. `ConstantPool.<init>`,
`ClassParser.readFields` and `readMethods` are invisible to *both* tier-up
counters: the method counter accumulates globally but they run once per class
(468 calls, under the 500 threshold), and the OSR trigger reads
`Frame::backward_count`, which is **per-frame and reset on every invocation**,
so a ~74-iteration constant-pool loop never approaches the 1000-back-edge
threshold *within one frame* — permanently, not as a warm-up artifact. The
stock scan reports `osr=0`: not one OSR body in the entire run.
`CRATONVM_JIT=loop-work-tierup` (default-off) fixes that — `ConstantPool.<init>`
compiles, tracked methods 9 → 10 — and buys **2–3%, inside the noise**.

**Not a field-resolution-cache miss either.** `resolve_field_ref_loader_aware`
does full symbolic work on every access *including a cache hit* — two `String`
allocations, two extra `class_manager.read()`s and a whole
`resolve_class_loader_aware` — purely to revalidate the entry it already holds.
Short-circuiting that for non-loader-sensitive callers
(`CRATONVM_JIT=field-cache-fastpath`, default-off) measured **nothing**: off
1894–2047, on 1919–2019 µs/class over four interleaved passes.

> **RETRACTED 2026-08-04 — that lever was INERT and the null result says
> nothing.** It gated on `should_use_loader_initiated_resolution`, which begins
> `if loader_aware_resolution() { return true; }` — and that flag is **default
> ON** (`classloading/src/class_manager.rs:503`, consolidated there precisely so
> the three copies could not drift). The predicate is therefore unconditionally
> true and the fast path could never execute. The stated caveat ("I did not
> confirm the fast path fires") was the tell; it should have been a blocker, not
> a footnote.
>
> The predicate that actually splits the cases is `loader_sensitive`, which
> additionally requires the referencing class's loader to be `UserDefined`. The
> replacement (`CRATONVM_JIT=field-site-cache`) uses that one and ships a
> `CRATONVM_DBG=field-site` counter, so "did the lever fire" is answerable
> before anything is timed. This is the fifth inert-lever incident in this
> investigation; a lever now has to prove it fired before it is allowed a
> timing number.

**So it is not a gate at all — it is interpreter throughput.** The decisive
measurement: on a quiet host `--nojit` is *faster* than the default
(1911/1868 vs 1978/1952 µs/class). Compiled code contributes nothing to this
workload; compilation overhead slightly outweighs it. The `perf` profile is
correspondingly flat — `execute_frame_from_index` 11.6%, then a long tail at
1–4% each (`is_object_address` 4.2, `execute_instruction` 3.6, `memcmp` 3.5,
`resolve_field_ref_loader_aware` 3.3, `invoke_on_class_shared_inner` 3.1,
`execute_invokevirtual_cached` 2.8, `load_class_concurrent` 2.2,
`slot_for_exact` 1.6, `complete_jmx_monitor_enter` 1.5) — no hotspot to remove.

### The number that actually sizes this: interpreter vs interpreter

`CallFloorProbe` under `HotSpot -Xint` against `CratonVM --nojit` removes the
JIT from both sides and prices the interpreters directly (ns/op):

| body | HotSpot `-Xint` | CratonVM `--nojit` | ratio |
|---|---|---|---|
| arith (no call) | 17.2 | 176.1 | 10x |
| + invokestatic leaf | 21.1 | 435.7 | **21x** |
| + invokevirtual leaf | 17.2 | 666.0 | **39x** |
| + invokeinterface leaf | 17.0 | 679.7 | **40x** |
| + `String.length()` | 26.6 | 1105.1 | **42x** |

Read the *increments*, not the absolutes: adding one call costs HotSpot's
interpreter ~4 ns and CratonVM's **~260 ns (static) to ~490 ns (virtual)** —
**65–120x**. Pure arithmetic is only 10x. So this VM's interpreter is
respectable at straight-line bytecode and catastrophic at **invoke**, and the
BCEL parse is invoke-dense (one object per constant-pool entry, getters
throughout). That, not any one symbol, is the 226x.

Scale, from `CRATONVM_DBG=hotpath-counts` over the same scan: ~2.0M bytecodes
executed, ~1.2M instance-field accesses, 201k method-ref resolutions — about
5,300 bytecodes per class at ~385 ns each.

**What would actually move this** is the interpreted invoke and field paths.
Fixing them is an interpreter-dispatch project — inline caches, a resolved
constant pool, per-call-site precomputed flags — not a point fix, and it is the
only thing that gets 240x anywhere near the ~5x exit criterion.

#### Correction: it is not `try_stackless_invoke` (2026-08-04)

An earlier revision of this section named `try_stackless_invoke`'s ~34 per-call
string comparisons as the thing to fix. **That was wrong, and the way it was
wrong is worth keeping.** `CRATONVM_DBG=invokestats` over the scan:

```
[invokestats] cache_hit=600001 cache_miss=1784 vtable_fast=2672 slow_path=3970
```

The monomorphic inline cache is **99.7% warm**. `try_stackless_invoke` is the
cache-*miss* path; it runs on roughly 0.3% of invokes, so its comparison count
is irrelevant no matter how large. The claim was inferred from reading the
source rather than from asking how often the function executes — the same
mistake, in a different costume, as the four falsified root causes above.

The string comparisons per invoke are real, but they are on the **hit** path:
`intercept_force_registered_native_cached` runs a sequence of
`(method_name, method_descriptor)` matches on every inline-cache hit that
dispatches bytecode. That is a per-call-site precomputable question and is
where the "precomputed flags" half of the project belongs.

#### The clusters, by mechanism

Re-profiled at a 0.35% floor on a quiet host (2039.7 / 1976.2 µs/class):

| cluster | share | mechanism |
|---|---|---|
| dispatch loop | ~13.5% | `execute_frame_from_index` 8.92, `execute_instruction` 3.38, `execute` 1.19 |
| **field resolution** | **~12%** | `resolve_field_ref_loader_aware` 3.51, `load_class_concurrent` 2.59, `resolve_class_loader_aware` 2.02, plus its share of `memcmp` 4.30, `sip::Hasher` 0.48, `_mi_page_malloc_zero` 1.10 / `mi_free` 0.79 |
| invoke + frame | ~12% | `execute_invokevirtual_cached` 2.90, `pop_and_recycle_frame` 1.62, **`drop_in_place<Option<(Arc.., Arc<str>, Arc<str>, usize)>>` 1.54**, `InvokeCache::get` 1.23, `Frame::new_pooled_cached` 1.05 |
| native registry | ~4.2% | `slot_for_exact` 2.50, `should_force_registered_native_over_bytecode` 0.75, `slot_index_for_key` 0.53, `intercept_force_registered_native_cached` 0.40 |
| heap checks | ~6.5% | `is_object_address` 3.78, `record_object_ref_payload_slow` 1.05 |

Two of those have an identified, removable mechanism rather than just a name:

* **Field resolution.** `resolve_field_ref_loader_aware` re-derives the
  field-*owning class* from its name on **every** access, resolution-cache hit
  included: two `String` allocations, three `class_manager` read acquisitions
  and a full `resolve_class_loader_aware`. Only the second half of the answer
  (locate the field in the owner) is memoized. ~1.2M accesses.
* **That 1.54% `drop_in_place`.** It is `resolve_method_ref`'s four-value return
  being dropped. `pop_coerced_invoke_args_virtual` / `_static` call it on the
  inline-cache **hit** path for two of those four values — the descriptor and
  the parameter count — paying a `resolution_cache` read lock, a hash probe and
  three `Arc<str>` clone/drop pairs per invoke to get one string and one
  integer.

Both are answered by the same thing: a per-thread, epoch-validated resolved
constant pool (`vm/src/runtime/interpreter/site_cache.rs`), behind
`CRATONVM_JIT=field-site-cache` and `CRATONVM_JIT=method-site-cache`.

#### Real-application validation: 351 Spring Boot test classes (2026-08-05)

The regression suite and the two dedicated vectors do not answer "is this safe
on a real, loader-heavy application". Spring Boot's own unit tests do. Corpus:
351 compiled test classes from `core/spring-boot`, driven one process per class
through a JUnit Platform launcher harness, both arms.

**Result: 346/351 vs 347/351 rc=0, and all 5 divergent classes are FLAKY IN BOTH
ARMS**, not cache defects:

| class | evidence |
|---|---|
| `SpringApplicationTests` | 5 reps/arm: off `failed=0,0,8,0,0`; on `0,0,4,0,0` — flakes in both |
| `SpringApplicationBuilderTests` | 15 reps/arm, **alternating**: off bad 3/15, on bad 4/15 |
| `StringToPeriodConverterTests` | 5 reps/arm: 0/5 both; the original `containersFailed=1` was in the OFF arm |
| `DefaultSslManagerBundleTests` | 5 reps/arm: 0/5 both; original `failed=2` was in the OFF arm |
| `ThreadPoolTaskSchedulerBuilderTests` | 5 reps/arm: 0/5 both |

The divergences were **bidirectional** — 3 classes did better with the cache on,
2 worse — which is the signature of flakiness, not of a defect. A cache serving
a wrong field is one-directional and deterministic.

The lever demonstrably fires on this corpus (`hit=184126` on one test class), so
this is not a vacuous green.

**Two honest caveats.**

* **The 351-class run was blocked, not interleaved** (`off` × 351, then `on` ×
  351). The off arm ran at host load 16–30 and the on arm after load dropped,
  which systematically favours ON for load-sensitive flaky tests — and 3 of the
  5 divergences favoured ON. That is a violation of this repo's own interleaving
  rule; the follow-ups above alternate arms per repetition, which is what makes
  the 3/15-vs-4/15 comparison trustworthy.
* **This does NOT validate `field-site-cache-loader`.** Every site in this
  corpus is loader-blind (`reject_loader` ≈ 0 — the tests run off a plain
  classpath), so the loader arm was inert here. It remains unvalidated on real
  loader-heavy code, which is exactly where it is supposed to matter.

#### Sizing: NOT a sizing problem — the misses are compulsory, not conflict

The hit rate that makes the scan number possible does **not** generalise:

| workload | hit | miss | rate |
|---|---|---|---|
| Tomcat annotation scan | 1,329,432 | 1,234 | **99.9%** |
| `SpringApplicationShutdownHookTests` | 184,126 | 166,035 | **53%** |

> **Retracted the same day.** I first read `fill ≈ miss` as "the 1024-slot table
> is thrashing" and added `CRATONVM_JIT=field-site-slots=N` to fix the size.
> **The sweep says the size is not the problem.** Hit rate against slot count,
> `CRATONVM_DBG=field-site` (hit rate is load-independent, so this is valid on a
> loaded host):
>
> | slots | `SpringApplicationShutdownHookTests` | `ConfigDataActivationContextTests` | annotation scan |
> |---|---|---|---|
> | 1024 | 52% | 87% | 99% |
> | 4096 | 52% | 88% | — |
> | 16384 | 52% | 88% | 99% (miss 4055 → 367) |
> | 65536 | **52%** | 88% | — |
>
> A **64x** larger table moves Spring Boot by nothing. So those are **compulsory
> (cold) misses, not conflict misses**: each site is touched once or twice and
> never reused. `fill ≈ miss` is equally consistent with cold misses — every
> first touch fills — so it never distinguished the two, and I named the
> mechanism before checking capacity.
>
> **Confirmed independently on the Windows box**, different hardware and
> different workloads (`--nojit`, hit rate at 1024 → 65536 slots):
>
> | workload | 1024 | 65536 | hits |
> |---|---|---|---|
> | `SiteCacheCostProbe` | 99% | 99% | 32.2M |
> | `RSerial` | 89% | 90% | 24.4k |
> | `RCollections` | 81% | 81% | 244 |
> | `RStrings` | 71% | 71% | 686 |
> | `RReflect` | 55% | 55% | 128 |
>
> Flat everywhere; only `RSerial` moves, by one point. (The last three rows have
> too little volume to say anything about those workloads individually — the
> conclusion rests on the high-volume rows here and on Azure.) **1024 is
> sufficient; a bigger table buys nothing on any workload measured.**

What the numbers actually say is the ordinary thing a cache says: **it pays
where there is REUSE and is neutral where there is not.** The annotation scan
re-executes a small set of field sites 1.3M times (99%); a short-lived unit-test
class executes a large set a handful of times each (52%). Both are correct
behaviour, and it matches the independent-vector result above — parity on
boot-dominated runs, 12.7% on the scan.

**This is why the default stays OFF:** not because the size is wrong, but
because the benefit is *workload-shaped*, and the broad-code case is still
unmeasured in time. `field-site-slots` is kept as a diagnostic — it is the only
cheap way to tell a conflict miss from a compulsory one on a future workload —
with its default unchanged at 1024, which the sweep confirms is sufficient.

#### What those two levers are worth — ON THE SCAN (2026-08-05)

The authoritative measurement: real BCEL, real JARs, Azure host, load steady at
**1.39–1.47** for the whole run. Four interleaved passes, arm order reversed on
even passes, HotSpot control every pass. `us/class`, both `parse` readings per
pass:

| arm | pass1 | pass2 | pass3 | pass4 | mean |
|---|---|---|---|---|---|
| off | 1845.2 / 1821.8 | 1884.3 / 1822.1 | 1855.9 / 1807.0 | 1878.5 / 1818.0 | **1841.6** |
| `field-site-cache` | 1623.1 / 1577.6 | 1639.5 / 1592.1 | 1616.0 / 1585.6 | 1632.7 / 1596.8 | **1607.9** |
| + loader arm | 1615.4 / 1595.1 | 1617.7 / 1601.8 | 1614.2 / 1584.5 | 1608.7 / 1583.4 | **1602.6** |
| + `method-site-cache` | 1598.0 / 1585.2 | 1604.2 / 1577.1 | 1606.1 / 1583.9 | 1622.8 / 1589.6 | **1595.9** |
| HotSpot | 7.4 / 6.2 | 6.7 / 6.8 | 8.3 / 7.7 | 7.2 / 6.6 | **7.11** |

**12.7% off the scan for `field-site-cache` alone; 13.3% with all three.** The
separation is total: every one of the 8 `off` readings is 1807–1884, and every
one of the 24 lever readings is 1577–1640. No overlap, in either order, on a
quiet host.

Against HotSpot that is **259x → 224x**. Real, and nowhere near the ~5x exit
criterion — which is the point the rest of this document makes.

Structural check, same run — the lever fires 1.33M times on one scan:

```
off:    field: hit=0        miss=0    | method: hit=0
field:  field: hit=1329432  miss=1234 | method: hit=0
fieldl: field: hit=1329436  miss=1234 | method: hit=0
all:    field: hit=1329434  miss=1234 | method: hit=52566
```

`reject_loader=0` throughout: this probe runs off a plain classpath, so every
site is loader-blind and the loader arm has nothing extra to admit — which is
why it adds only 0.3%. Inside a real webapp deploy, where the scanning code runs
under a user-defined loader, that arm is what keeps the base arm from being
inert; `reject_loader` is the counter that will say so.

`method-site-cache` adds 0.4% on top (1602.6 → 1595.9) — consistent with the
"measures nothing" verdict below, marginally positive rather than negative.

#### The mechanism's own price, in isolation

Measured separately with `probes/SiteCacheCostProbe.java`, which is deliberately
field-saturated, so its ratio is the *mechanism's* headroom and not the scan's.

Structural check first — both levers demonstrably fire, which is the thing the
retracted measurement above never established:

```
off:    field: hit=0         | method: hit=0
field:  field: hit=12900001  | method: hit=0
method: field: hit=0         | method: hit=900043
both:   field: hit=12900001  | method: hit=900043
```

`--nojit`, ns/op, mean of the last two rounds across four passes:

| benchmark | off | field-site-cache | method-site-cache | HotSpot `-Xint` |
|---|---|---|---|---|
| field-heavy | 43,630 | **21,170** (1.9–2.7x per pass) | 49,900 | ~270–440 |
| mixed | 43,331 | **23,470** | 45,850 | ~425–520 |
| native-call-heavy | 5,467 | 5,282 | 5,915 | ~400–480 |

* **`field-site-cache` is worth ~1.9x on interpreted field-heavy code**, and the
  ratio holds in every pass in both orders (1.94, 1.84, 2.69, 1.97). It cuts the
  interpreter-vs-interpreter gap on this shape from ~100–160x to ~50–79x.
* **`method-site-cache` measures nothing.** It removes real work — a
  `resolution_cache` read lock, a hash probe and three `Arc<str>` clone/drop
  pairs per invoke — but that work is small beside the `safe_native_call` funnel
  a native invoke pays anyway (~450 ns/call here). The counter proves it fires
  900k times and it still does not show. Kept, default-OFF, on the same footing
  as the other measured-nothing levers: the waste is real, the payoff is not.

In **default (JIT-on)** mode neither lever shows a reliable difference on this
probe (field-heavy: off ~2,570, field ~2,466, both ~2,233 ns/op, inside the
spread) — the JIT compiles the loop and never touches the interpreter's field
path. That is consistent rather than contradictory: this workload is only
interesting because the annotation scan is a case where **the JIT contributes
nothing** (`--nojit` is *faster* there, measured above), so the scan sits in the
first regime, not the second.

Correctness: 28/28 regression suite green with the levers off and with both
levers plus the loader arm on. Two dedicated vectors — `RFieldSiteCache` (291
checks) and `RMethodSiteCache` (44) — target the silent failure modes
specifically, since every way these caches can be wrong returns a plausible
number rather than throwing.

#### The purpose-built probe hid a regression; independent vectors found it

The first version of the cache held **one** epoch pair for the whole table and
wiped all 1024 slots when either moved. `class_definition_epoch` advances on
*every class definition*, so during start-up and any class-loading burst it
moves constantly — and each field access was then paying an `O(SLOTS)` memset.

`SiteCacheCostProbe` never showed this, because it reaches steady state and
stops defining classes. Six regression vectors that never reach steady state
(~500 ms runs, boot-dominated) did, and they were **uniformly slower** with the
lever on:

| vector | off | on (table-wide wipe) | on (per-entry epochs) |
|---|---|---|---|
| RCollections | 499 | 629 | 664 vs 666 off |
| RStrings | 563 | 621 | 675 vs 628 off |
| RSerial | 572 | 673 | 781 vs 781 off |
| RExceptions | 526 | 623 | 669 vs 698 off |
| RReflect | 552 | 592 | 709 vs 675 off |
| RNumbers | 581 | 679 | 693 vs 713 off |

Holding the epoch pair **per entry** removes the wipe entirely: an epoch change
costs nothing, stale entries miss one at a time and are replaced in place, and a
hit is one array index plus four integer compares on a single cache line. After
that change the six vectors are at parity — the correct outcome for a
boot-dominated run, where the cache should not help and must not hurt.

Two things worth keeping from this:

* **A probe written alongside a fix will tend to exercise the shape the fix is
  good at.** The independent check was what caught it, and it is cheap.
* **A cache's invalidation cost is part of its cost.** An `O(n)` wipe keyed on a
  counter that moves during class loading is not a cache, it is a memset with a
  lookup attached.

Re-measured after the redesign the field arm still lands in the same band, but
that run was taken on a box no longer idle (the `off` column spread 38k–127k
against 37k–48k on the clean run), so **the 1.9x above is the clean-run figure**
and the re-measurement should be read only as confirming direction and
magnitude, not as an independent estimate.

**Consequence for the exit criteria below: they are not reachable by tiering
work.** ~240x against an interpreter that the JIT cannot help is a
general-throughput problem. Anyone picking this up should either attack
interpreter dispatch cost broadly, or re-scope the exit criteria.

Still true from the original triage:

* Not the `seek0`/`ExpandWar` defect — that is fixed and verified separately;
  the `ExpandWar` error no longer appears in these runs.
* Not JAR/zip/inflate throughput (under 2x, measured above).
* Not GC and not a hang — the deploys complete, just late.

## Measuring this at all

The Azure build host is shared, and during this investigation its load ran
between 6 and 178. The **same binary and configuration** measured 2237 and
14789 µs/class an hour apart, and the HotSpot column swung 13.5 → 99.0 µs/class
across three interleaved rounds. Any number in this doc taken at load > 10 is
noise. Check `/proc/loadavg` first; interleave the arms in both directions; and
prefer CratonVM-vs-CratonVM A/B over the cross-VM ratio.

Two levers here are **partially inert**, which is worse than useless because
they read as clean negatives:

* `CRATONVM_JIT=threshold=N` moves the *counting-site* threshold but **not**
  the tiered manager's own — `jit-method-stats` still prints
  `c1_threshold=500` at `threshold=10`.
* Crossing the invocation threshold does not by itself nominate anything: the
  only site that acts on the counter is the dispatch site, and it tests
  `cnt == threshold || (cnt - threshold) % 64 == 0` against the value **its
  own** increment returned. `CRATONVM_DBG=loop-work` was added to make this
  visible — it caught `ConstantPool.<init>` sitting at a count of **1149**,
  more than twice the threshold, still never nominated.

## Prior art

This is the same wall
`tomcat/04-embedded-server-throughput-wall-CLOSED.md`
measured on 2026-07-27 (it recorded 13–16 µs per byte for the identical
`DataInputStream`/`BufferedInputStream` operation; today's
`ByteReadCostProbe` reads 15.0 µs) and handed to
`31-synchronized-code-never-jit-compiled-FIXED.md`. Doc 31's fix landed on
2026-07-28 and did not move this path. It is also the residual that
`fixed-suite-bugs/managerwebapp-deploy-bare-assertion-FIXED.md`
retired against in July, naming these same two test methods.

The admission bans that plausibly own it are catalogued in the retired
`30-hot-loop-jit-admission-bans-testmethodperformance` write-up (RBC.6
`local_handler_reads_unsafe_local`, RBC.7 `invokedynamic` OSR denial, and the
`<init>` complexity ban). **They are load-bearing** — each closed a real
silent-wrong-results bug — so lifting any of them needs the corruption
regression suite plus a full Tomcat+Spring+Hibernate soak, not a local
measurement.

## Reproduction

Standalone, no Tomcat startup, ~2 s per run:

```bash
cd C:/craton/CratonVM/apps/tomcat
javac -cp "$(cat .suite/cp.txt)" -d /tmp/probeout probes/AnnotationScanCostProbe.java
<cratonvm.exe> --java-home "<real JDK 25>" -Xmx2g \
  -cp "/tmp/probeout;$(cat .suite/cp.txt)" \
  AnnotationScanCostProbe output/build/webapps/examples/WEB-INF/lib
```

Full class (≈230 s CratonVM, ≈12 s HotSpot):

```bash
pwsh apps/tomcat-suite-runner/run-one.ps1 -Vm craton -Exe <cratonvm.exe> -Class org.apache.catalina.manager.TestManagerWebapp
```

## Exit criteria

> ⚠️ **Not reachable by tiering work** — see § What this is NOT — measured.
> Every JIT-admission and tier-up lever tried on 2026-08-04 moved this by ≤3%,
> and `--nojit` is *faster* than the default, so the remaining distance is
> interpreter throughput. Closing this doc means either a broad interpreter
> dispatch improvement or a re-scoped criterion.
>
> **Progress 2026-08-05: 259x → 224x** (12.7–13.3%) from the interpreter's
> resolved constant pool, `CRATONVM_JIT=field-site-cache` (still default-OFF).
> That is the first lever in this investigation to move the number outside
> noise, and it confirms the diagnosis — the win came from deleting per-access
> symbolic work, not from compiling anything. It also sizes what is left: at
> 224x, reaching ~5x needs roughly another 45x, which no cache on the field path
> can supply. The remaining mass is the dispatch loop itself
> (`execute_frame_from_index` + `execute_instruction` ≈ 13.5%), frame push/pop,
> and the per-invoke native-registry and heap-check work — i.e. a genuine
> interpreter rewrite, or a re-scoped criterion.

`AnnotationScanCostProbe` within ~5x of HotSpot per class, which should bring
the `examples` redeploy under the ~1 s the `list` assertion needs and the
`bug57700` deploy under the client's 30 s read timeout. Both test methods then
pass without touching the test.

---

## Handoff 2026-08-07 — the `TestHostConfigAutomaticDeployment*` family is this doc's, and here is its profile

Arrived here from
`fixed-suite-bugs/tomcat/gc-moving-young-persistent-nonmoving-fallback-regression-CLOSED.md`,
which proposed that a persistent moving-young → non-moving GC fallback was
making that family HANG. It is not: GC-side work on those classes measures
**0.4 %** of the run (101 minor collections, 0 major, 1.41 s of card refinement
in a 352 s run), and forcing `CRATONVM_NO_MOVING_YOUNG=1` changes nothing. The
family does not hang either — all ten classes PASS. What is left is this doc's
subject, so the measurements move here.

**Scale, standalone, HotSpot control run back to back on the same host**
(`dev` `e9c05391a`; box quiet at load ~15 for the last row):

| Class | CratonVM | HotSpot | ratio |
|---|---|---|---|
| `…DeploymentModification` | 448 s | 21 s | 21× |
| `…DeploymentUpdateWarOffline` | 246 s | 13 s | 19× |
| `…DeploymentDeleteC` | 215 s | 13 s | 17× |
| `…DeploymentCopyXML` | 126 s | 10 s | 12× |
| `…DeploymentDeleteA` | 77 s | 7 s | 10× |
| `catalina.nonblocking.TestNonBlockingAPI` | 485 s | 55 s | 9× |
| `…DeploymentAddition` (quiet box) | 352 s | 11 s | **32×** |

**Time-weighted interpreted profile** (`--stack-sample-ms 50` over `CopyXML`,
1516 samples ≈ 76 s of a 79 s run — essentially every sample has an interpreted
frame on top):

```
 55.21%  org/apache/tomcat/util/bcel/classfile/ConstantPool.getConstant
 12.47%  java/io/BufferedInputStream.fill
  5.74%  java/io/BufferedInputStream.read
  2.97%  org/apache/catalina/startup/ContextConfig.processAnnotationsJar
  2.64%  org/apache/tomcat/util/bcel/classfile/ConstantPool.<init>
  1.65%  org/apache/catalina/startup/ContextConfig.processResourceJARs
  1.52%  org/apache/catalina/startup/ContextConfig.processAnnotationsFile
  1.19%  org/apache/tomcat/util/bcel/classfile/JavaClass.<init>
  0.86%  java/io/BufferedInputStream.getBufIfOpen
```

The `BufferedInputStream` bodies this doc's 2026-08-06 update named are still
there but no longer dominant (≈ 19.5 % combined, down from 78.3 %). The new top
line is BCEL's own constant-pool reader.

**Native-invocation census** (`--dump-native-registry`, same class, 122 s):
**48.2 M native invocations**, and the shape is the class-file reader:

```
12 645 096  java/io/DataInputStream.readByte()B
 6 935 427  java/util/Objects.requireNonNull(Object,String)
 6 934 818  java/io/DataInputStream.readUTF()
 6 523 648  java/io/DataInputStream.skipBytes(int)
 4 802 416  java/io/DataInputStream.readUnsignedShort()
 1 771 570  java/lang/Class.isAssignableFrom(Class)
 1 771 319  java/lang/Class.cast(Object)
 1 490 869  java/io/DataInputStream.readInt()
   746 864  java/util/jar/JarEntry.getName()
```

That is ≈ 2.5 µs of wall per native invocation if the run were nothing else,
which it is not — but it does say where to look next: **the per-invocation cost
of a registered native, and the `DataInputStream` family's 32 M round trips**,
not the JIT-admission levers this doc has already exhausted.

### One lever measured and rejected as a fix

`ConstantPool.getConstant(int, Class)` runs `castTo.isAssignableFrom(…)` and
`castTo.cast(…)` once per constant-pool access — the 1.77 M pairs above. Both
natives materialised class **names** before deciding (two `mirror_class_name` /
`class_name_of_id` calls each, every one taking the class-manager read lock and
cloning a `String`). Both now answer the "same class, or a subclass" shape from
class ids alone. Measured, 2 M iterations, A-B-B-A:
`isAssignableFrom` 793 → 565 ms (1.40×), `isInstance` 899 → 615 ms (1.46×);
HotSpot is 9 ms and 7 ms.

**It does not move this workload**: 1.77 M × 114 ns ≈ 0.2 s of 122 s, and
`CopyXML` measures the same before and after (A-B-B-A: 116 / 111 / 116 / 122 s).
Recorded so the next reader does not re-derive it — the reflective type checks
are 1.5 % of the native traffic here, and the `DataInputStream` family is 66 %.
