# i110-L4: the JDK's `invokeWithArguments` body costs ~35 us a call here (HotSpot ~1-3 us)

**Status: OPEN (diagnosed by reading; census scope built, not built or run).** Filed by interpreter
round i5 wave 110, lane L4.

## Symptom

`tools/probes/interp/L4/L4W109InvokeWithArgumentsBodyBench.java` with `CRATONVM_MH_IWA_JDK_BODY=1`
(w109, Windows): `static-int` 1385 -> 37485 ns, `list` 2916 -> 44145 ns, `bound` 2.5x,
`varargs-control` flat. So the switch stays OFF, and the `invokeWithArguments hidden` row of
`gap-hidden-walks-lack-hotspots-spun-lambda-form-frames` stays a gap. Any application whose
`invokeWithArguments` reaches the JDK body pays the same cost (the served door's decline path, and
the `AsVarargsCollector` override).

## HotSpot behaviour

The body is `genericMethodType(n)`, `asType`, then
`invokers().spreadInvoker(0).invokeExact(adapted, args)`. Every object in it is cached by the JDK.
The chain is then about six `LambdaForm` steps; interpreted it costs ~1-3 us, and compiled it
costs nanoseconds.

## Where the time goes (by reading)

**Not a cache miss.** Each step of the body hits a JDK cache in this VM too:

* `MethodType.genericMethodType(n)`: the `objectOnlyTypes` table.
* `invokers()`: the `MethodType.invokers` field.
* `spreadInvoker(0)`: `Invokers.spreadInvokers[0]`.
* `asType(generic)`: the receiver's `asTypeCache`. The registered `asType` `Intrinsic`
  (`lang_invoke.rs`, `mh_foreign_as_type`) runs the JDK's `asType` bytecode, which reads that
  cache, and `isSafeToCache((Object..)Object)` holds.
* The spreader's `checkGenericType`: the same type, so it answers the handle itself
  (`method_types_same_classes`).
* `checkCustomized`: the handles are the same objects on every call, so customization happens
  once.

**It is the chain itself, at ~5-6 us per step.** The chain runs as follows:

1. The `invokeExact` door (`mh_linked_invoker`, a pair-hit: the site
   `(MethodHandle,Object[])Object` is shareable).
2. `native_method_handle_link_to`.
3. `Invokers$Holder.invokeExact_MT`.
4. `invokeBasic`, into the spread/generic-invoker `LambdaForm$MH` (spun, hidden).
5. The `asType` `Intrinsic` crossing.
6. `invokeBasic`, into the adapter `LambdaForm$MH` (spun, hidden).
7. `invokeBasic`, into `DirectMethodHandle$Holder.invokeStatic`.
8. `linkToStatic`, into the target.

That is six or seven native crossings, each a Java re-entry (`invoke_static_settling_keyed` ->
`interpreter::execute`). i95 measured the same per-step cost on a much shorter chain:
`bound-getter-warm-exact` took ~19 us for `invokeExact_MT` -> bound reinvoker -> `DMH$Holder.getInt`.
Both chains cost about the same per step, so the cost is the re-entry, not anything specific to
`invokeWithArguments`.

**The JIT cannot link the chain.** `mh_compiled_link_redirect` (`vm/src/jit/helpers.rs`) refuses a
target in a hidden `LambdaForm$MH` class: its ladder resolves names, and the spun class is not
name-resolvable. So even compiled `LambdaForm` bodies cross the native door at every `invokeBasic`.
The census will say whether the spun classes are compiled at all (`reentry-callee.iwa-body` states).

## Census (built in wave 110)

`CRATONVM_DBG=mh-finder-census` now has a named scope `iwa-body` around the JDK body that
`mh_iwa_jdk_body` runs (`native-builtins/src/mh_finder_census.rs`, `named_scope`). It records:

* `timed scope iwa-body`: the body's total time. A `List` row counts its nested array body too.
* `reentry.iwa-body <method>`: re-entries by method.
* `reentry-time.iwa-body <method>`: outermost re-entry time.
* `reentry-callee.iwa-body <state> <method>`: the JIT state of each re-entered callee
  (`published`, `bail-listed`, `hot-unpublished`, `cold`).
* `jit-callee-door.iwa-body`, and with `CRATONVM_DBG_INTERP_FRAMES=1` `interp-frames iwa-body`
  (frames per body).

```text
CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_DBG=mh-finder-census CRATONVM_DBG_INTERP_FRAMES=1 \
  cratonvm --jdk-only -cp . L4W109InvokeWithArgumentsBodyBench 2000 static-int 2> iwa-census.txt
grep -E 'iwa-body|linker invokeExact' iwa-census.txt
```

How to read it:

* **Reentries per body ~6, `reentry-time` ~5 us each:** the per-re-entry cost (L7/L2: the
  `interpreter::execute` entry), as in i95.
* **`cold` / `bail-listed` spun `LambdaForm$MH.invoke` callees:** the spun forms never compile.
  That is the JIT's hidden-class admission (L2).
* **Frames per body in the hundreds:** something in the chain is being interpreted that should not
  be. The rows name it.

## What a fix needs

Make a native-to-Java re-entry cheap, or let compiled `LambdaForm` code call its `invokeBasic`
target directly when that target is a spun hidden class (`mh_compiled_link_redirect` by class id
instead of by name). Both are outside L4's files. Until then the served door stays (switch OFF),
and the hidden-frame row stays a gap.

## Census run (interpreter round i5 wave 113, 2026-10-17)

> **VERIFIED AGAINST A BINARY 2026-10-17.** The census scope runs on Windows v2 (`35be28dd0e`).
> This verifies the census only; the page stays open.
> `CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_DBG=mh-finder-census CRATONVM_DBG_INTERP_FRAMES=1
> cratonvm --jdk-only L4W109InvokeWithArgumentsBodyBench 2000 static-int`: TIME `static-int`
> median 63 447 ns/op (machine shared with other builds). 16 000 bodies, 83 352 re-entries (about
> 5.2 per body). Outermost re-entry time: `invokeExact_MT` 33.7 us each (16 000), `asType`
> 11.0 us each (16 000). Callee state: `MethodHandle.asType` is `cold` on all 16 000
> re-entries. The spun `LambdaForm$MH.invoke` forms and `Invokers$Holder.invokeExact_MT` are
> `published` on about 15 400 of 16 000 (`cold` on the first ~500). So the per-re-entry cost
> (the i95 shape) is the main cost. The never-compiled `asType` re-entry is the second
> candidate; the spun forms do compile.

## Wave 114 (interpreter round i5, lane L4)

**OPEN; the `asType` re-entry is removed behind a default-OFF switch (not built or run by the
lane).**

### Why `MethodHandle.asType` stays `cold` (by reading)

It is neither a counter bug nor a refusing door. `MethodHandle.asType` carries its own registered
`Intrinsic` (`native-builtins/src/lang_invoke.rs`, the `asType` registration, needed for the FFM
downcall carrier). `interpreter::execute`'s first-call JIT block therefore computes `native_skip`
(`native_override::registered_native_will_run`, `vm/src/runtime/interpreter.rs` ~2574) and seals
the method `calls-native-shadowed-method` before the invocation counter is touched. The JDK body
that `mh_foreign_as_type` re-enters (`invoke_virtual_bytecode_only` -> `interpreter::execute`) is
thus interpreted on every call, forever: the census's `cold` on all 16 000. The same holds for every
method this VM shadows with an `Intrinsic` and then re-enters "bytecode only" (`invokeWithArguments`
itself under `CRATONVM_MH_IWA_JDK_BODY`). That is by design (`native_skip` must hold for the
dispatch path), so the fix is to not re-enter.

### Built: `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` (`truthy_word`, default OFF)

`NativeFlags::mh_foreign_as_type_cache_read` (`types/src/flags.rs`), `types/src/flag_groups.rs` row,
`types/tests/flag-surface.txt` line. In `lang_invoke.rs`:

* `mh_foreign_as_type_cache_hit`: the JDK's `asTypeCached` first test as two field reads
  (`asTypeCache` non-null, its `type` the requested `MethodType` by identity) answers that adapter.
  The JDK body answers the same object with no side effect, so it is Java-invisible. The
  `asTypeSoftCache` arm stays the JDK's (`SoftReference.get` updates the reference's clock).
* `mh_foreign_as_type` (the `asType` `Intrinsic` for a real JDK handle) asks it after its
  same-type answer and before the re-entry. That is the call the `iwa-body` census measured
  (11.0 us, once per body).
* `mh_real_as_type_bytecode`: the same two pure-read answers (`newType == type`, then the cache)
  before the JDK's `asType` bytecode, now used by the three natives that called that bytecode
  directly: the served `invokeWithArguments` door (`mh_real_iwa_adapted`), the
  converting `invoke` adaptation (`mh_real_invoke_adapt`) and the varargs collector
  (`mh_foreign_varargs_collect_raw`).

### Expected A/B rows

`L4W109InvokeWithArgumentsBodyBench` (`--jdk-only` and `--compatible`, JIT and `--nojit`,
interleaved medians):

* With `CRATONVM_MH_IWA_JDK_BODY=1`, switch off vs on: `static-int`, `static-ref`, `virtual`,
  `bound` should each drop by about the `asType` re-entry (~11 us at the w113 census load, about a
  sixth to a third of the row); `list` by the same (one array body per call);
  `varargs-control` flat (stays on the door).
* Without `CRATONVM_MH_IWA_JDK_BODY` (the served door): small or flat. The served door has its own
  adapter memo ahead of the `asType` call, so the cache read only helps its memo misses.
* The census with the switch on should lose the `reentry.iwa-body ... asType` rows:
  `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ=1 CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_DBG=mh-finder-census
  cratonvm --jdk-only -cp . L4W109InvokeWithArgumentsBodyBench 2000 static-int 2> c.txt; grep iwa-body c.txt`.
  About 4.2 re-entries per body should remain, all of them the `invokeExact_MT` / `invokeBasic`
  chain.

The `invokeExact_MT` chain (33.7 us) is still the L7 re-entry cost times its `invokeBasic`
crossings (L2: `mh_compiled_link_redirect` by class id). So `CRATONVM_MH_IWA_JDK_BODY` stays off
even after this switch, and this page stays open on those two owners.

### Probe

`tools/probes/interp/L4/L4W114AsTypeCacheRead.java`: the identity answers `asType` gives (own
type, strong cache hit, eviction by another type, soft-cache arm, warm loops, `invokeWithArguments`
both overloads, null and bad types). It must print its `HOTSPOT_EXPECTED` block in both arms of
the switch, both modes, and with `CRATONVM_MH_IWA_JDK_BODY=1` added.

### Check first

It compiles (the `mh_real_invoke_adapt` hunk matches on the `Value` that `mh_site_method_type`
answers and calls the helper through `&mut *scope`). Then the probe is `SAME` in every arm, and
the MH battery (`tools/probes/interp/L4/*`) with the switch on.

## Wave 114 (lane L7): the re-entry half

Not built or run. The per-re-entry cost (the census's ~5 us) is now itemized in
`interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224.md`: by reading, `interpreter::execute` and its
door pay a few hundred ns, not microseconds, and the census's own per-re-entry bookkeeping
(two `format!`s and a mutex in `execute_clock`, a JIT-cache lookup in the callee-state note)
inflates the wave-113 figure (63 us/op censused against ~35 us/op). The census now splits each
re-entry into `reentry-prologue.iwa-body` / `reentry-epilogue.iwa-body` (`execute`'s own time
around the body) and `reentry-nobody.iwa-body`; run the census command above and
`grep -E 'reentry-(prologue|epilogue|nobody)'` first. A default-off
`CRATONVM_EXECUTE_LEAN_REENTRY` drops the per-entry source-file and empty-table copies;
bench `tools/probes/interp/L7/L7W114NativeReentryBench.java`.

## Wave 115 (interpreter round i6, lane L7): the re-entry half

**SWITCH-OFF** (the re-entry half; the page stays OPEN). Not built or run. Read end to end again,
`interpreter::execute`'s compiled-return path has one cost that grows with the class rather than
staying constant: `Class::find_method`'s linear name/descriptor scan, and every step of this
chain re-enters a `*$Holder` class (`DirectMethodHandle$Holder` declares 217 methods in JDK 25,
`DelegatingMethodHandle$Holder` 134, `Invokers$Holder` 62). New default-off switch
`CRATONVM_EXECUTE_METHOD_INDEX_MEMO` serves the declared index from a per-VM memo, verified by
name and descriptor (`interpreter.rs::find_declared_method_memoized`). Wave 115 also fixed a
per-call source-file copy when `CRATONVM_EXECUTE_LEAN_REENTRY` and
`CRATONVM_EXECUTE_CACHED_REENTRY_FRAME` are both on, and names one more suspect to read off the
binary (the size of `execute`'s own stack frame). Details, probes
(`L7W115MethodIndexMemo`, `L7W115ReentrySourceFile`) and the A/B commands:
`interpreter-i114-L7-native-reentry-cost-split-KEPT-OFF-20261224.md`, "Wave 115". The A/B row here is
`L4W109InvokeWithArgumentsBodyBench static-int` with `CRATONVM_MH_IWA_JDK_BODY=1`, switch off
vs on (expected: lower by the Holder scans of ~5 re-entries per body; not expected to close the
gap to HotSpot).

## Wave 115 (interpreter round i6, lane L4)

**OPEN.** No code change for this page in wave 115. This section reviews the wave-114 cut,
corrects the page's diagnosis, and gives the re-entry map after wave 114 with an owner for each
re-entry. The next L4 cut is filed as proposal
`interpreter-i115-L4-proposal-shadowed-jdk-bodies-compile-for-their-bytecode-only-reentries-KEPT-OFF-20261019.md`.

### Review of `CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ` (wave 114, never compiled)

No defect found, by reading. Checked against JDK 25.0.3's `MethodHandle.asType` / `asTypeCached`
(`src.zip`):

* **Compiles.**
  * `mh_real_invoke_adapt` passes `&mut *scope`. `NativeHandleScope`'s `DerefMut` target is
    `dyn NativeContext + 'a`, and the same coercion is already used at `vh_array_element_verify_why`.
  * `site_mt` is a `Value` (`Copy`), so both match arms are well-typed.
  * The flag is a `NativeFlags` field parsed with `truthy_word`, with a `flag_groups` row and a
    `flag-surface` line.
* **GC safety.**
  * Both helpers make only `get_field_by_name` reads (no allocation, no Java), and the answer is
    returned at once.
  * Every caller reads `this` / `new_type` after its last GC point: `mh_site_method_type`,
    `build_method_type_from_descriptor` and `genericMethodType` all run before the pin re-reads.
* **HotSpot semantics.**
  * The JDK body is `newType == type ? this : asTypeCached(newType)`, then `setAsTypeCache`.
    `asTypeCached` reads `asTypeCache` and compares its `type` by identity, then reads the
    `asTypeSoftCache` arm. The helpers make exactly the first two answers, which are pure reads.
  * The soft arm stays the JDK's: `SoftReference.get` writes the reference's `timestamp`.
  * A miss, a `null` type (the JDK's NPE comes from `asTypeUncached`'s `isConvertibleTo`) and a
    `WrongMethodTypeException` all still run the JDK body.
  * No subclass of `MethodHandle` declares a field named `type`, so the by-name read is
    `MethodHandle.type`. `asType` is `final`, so no override is skipped.
* **Small inconsistency (not changed).** The `mh-foreign-as-type-cache-read` (and
  `mh-iwa-jdk-body`) `flag_groups` rows carry `off_word: Some("0")`, which the field's doc reserves
  for default-ON knobs. It is harmless, since `truthy_word` reads `0` as off. It is left alone so
  as not to move the inventory counts.

### Correction: `spreadInvoker(0)` is not cached in JDK 25

The page's "every object in it is cached" is wrong for one step. JDK 25's
`Invokers.spreadInvoker(int)` is
`genericInvoker().asSpreader(Object[].class, n)` on every call; only `genericInvoker()` is cached
(`INV_GENERIC`). So every `invokeWithArguments` body runs `asSpreader` (`asSpreaderChecks`, an
`asType` that answers the invoker itself, `rebind`, `editor().spreadArgumentsForm`, which hits the
form's transform cache, `replaceParameterTypes`, and `copyWith`) and allocates a fresh spreader
`BoundMethodHandle`. The spun spreader *form* is still shared, so its compiled `LambdaForm$MH`
body is shared too. HotSpot does the same work but compiles and inlines it. A fresh spreader per
call also means the linker's `checkCustomized` never customizes it, in HotSpot or here.

### Re-entries per body after wave 114 (by reading; the w113 census counted 5.2)

1. **The body itself.** `mh_iwa_jdk_body` -> `invoke_virtual_bytecode_only`. It is interpreted
   for good: `MethodHandle.invokeWithArguments` carries its own `Intrinsic`, so the first-call
   JIT block seals it `native_skip`. Owner: L4 / L7, see the proposal above.
2. **The body's `invokeExact`.** It is interpreted, so it goes through the signature-polymorphic
   route to `mh_linked_invoker` -> `native_method_handle_link_to` -> re-entry into
   `Invokers$Holder.invokeExact_MT`. A *compiled* body would take
   `mh_compiled_link_redirect`'s `Linker` shape (a direct compiled call, no door) once the linker
   pair is filed. Owner: the same proposal (the body has to compile).
3. **`invokeBasic` into the spread invoker's form, and into the `asType` adapter's form.** Both
   come from compiled code. Both targets are hidden spun classes, which `bind_checked` refuses, so
   each goes through the door and a re-entry. Owner: L2, `interpreter-i115-L2-proposal-compiled-invoke-basic-linked-by-class-id-KEPT-OFF-20261224.md`.
4. `invokeBasic` into `DirectMethodHandle$Holder.invokeStatic` and `linkToStatic` into the target
   are name-resolvable and link compiled (no re-entry expected).
5. `asType`: removed by wave 114's switch (a cache hit).

So, with wave 114's switch on, four re-entries per body remain: two that L4 can remove (1, 2: the
body compiles) and two that belong to L2 (3). Each costs the L7 re-entry figure. The census command
in "Wave 114" above checks this count (`reentry.iwa-body` rows) once the switch is on.

## Wave 116 (interpreter round i6, lane L4)

**SWITCH-OFF** (two new default-off switches, both read only under `CRATONVM_MH_IWA_JDK_BODY`; the
page stays open for the L2 re-entries). Not built or run by the lane.

### `CRATONVM_MH_IWA_NATIVE_STEPS` (re-entries 1 and 2 of the wave-115 map)

`native-builtins/src/lang_invoke.rs` `mh_iwa_native_steps`, called first by `mh_iwa_jdk_body`
for the array overload. It runs the JDK array body's steps from the intrinsic instead of
re-entering its bytecode, which is interpreted forever:

* `asType(genericMethodType(n))` through `mh_real_iwa_adapted`: the JDK's own calls, memoised per
  epoch, plus wave 114's `asTypeCache` read. A throw declines, so the JDK body raises it with its
  own frame, as the served door does.
* The spread invoker through `mh_iwa_spreader`: the JDK's
  `genericMethodType(n).invokers().spreadInvoker(0)`.
* `invokeExact` through the JDK linker itself (`mh_linked_invoker` at the body's site type
  `(MethodHandle,Object[])Object`, falling back to `mh_invoke_exact_door`). So
  `Invokers$Holder.invokeExact_MT` and the spun forms are real activations.
* The `invokeWithArguments` frame is the served door's reflective mark
  (`with_invoke_with_arguments_frames`).

The `List` overload keeps its JDK body, whose array call comes back here. Varargs collectors and
downcall carriers are excluded upstream (`mh_iwa_jdk_body`).

### `CRATONVM_MH_IWA_SPREADER_MEMO` (the per-call `asSpreader`)

`mh_iwa_spreader` files the first spreader per `n` in the VM's rooted bound-type table
(`method_handle_linker_get` / `_put`, key `iwa-spreader:<n>` keyed by `MethodHandle`'s class id).
It is a GC root remapped by every collection, so no address epoch is involved, and it is shared by
the VM's threads. The spreader names boot classes only, so it retains no application loader. This
is invisible to Java:

* The spreader is only the linker's receiver and never escapes.
* Its `type` and `form` are the JDK's interned and cached objects, so a fresh call would build the
  same ones.
* Its `customizationCount` is set back to `0` before each reuse, so `invokeExact_MT`'s
  `checkCustomized` never customizes it, as on HotSpot, where the spreader is fresh on every call.
  Under concurrent threads the reset and the increment race, as HotSpot's own count updates on a
  shared handle do. At worst the spreader customizes once, which changes no answer.

The TARGET handle still customizes after ~127 calls. The generic invoker form calls
`checkCustomized` on it, on HotSpot too (the probe's `hidden-after-warm` row, `-Xint` the same).

### `vm_exec.rs::invoke_on_class_shared_inner` (L7's `$Holder` rescans)

Both changes are under L7's `CRATONVM_EXECUTE_METHOD_INDEX_MEMO`:

* The receiver-retarget arm asks `find_method_recursive` only when a retarget is possible (an
  interface or abstract `class_id` and a concrete receiver). Before, every static `$Holder`
  re-entry walked the table to compute a value the condition then ignored.
* The exact-class native arm finds the declared method through `find_declared_method_memoized`.

The main `find_method_recursive` walk further down is unchanged: it is recursive, and the memo is
per declared class.

### Probe and A/B

* `tools/probes/interp/L4/L4W116IwaNativeSteps.java` (`HOTSPOT_EXPECTED` from 25.0.3, default and
  `-Xint` identical). It covers arities 0-5, a null array, boxing, 500 warm calls (past the
  customization threshold on a reused spreader), a re-entrant nested `invokeWithArguments`, frames
  (default, hidden, hidden after warm), a thrown exception and its trace, and the
  `WrongMethodTypeException` and `ClassCastException` paths. Run it in four arms: switches off,
  `CRATONVM_MH_IWA_JDK_BODY=1`, plus `CRATONVM_MH_IWA_NATIVE_STEPS=1`, plus
  `CRATONVM_MH_IWA_SPREADER_MEMO=1`. Use both modes, JIT and `--nojit`. Compare the arms with each
  other first (the `hidden` rows are the known gap with the body off).
* Also: `L4W109IwaHiddenFrames`, `L4W114AsTypeCacheRead`, `L3W104WalkOptionsThroughHandles`,
  `L4W84InvokeWithArgumentsJumbo`, `L4W95IwaVarargsSuper`, `L4W98SpreaderConversion` in the last
  arm.
* A/B: `L4W109InvokeWithArgumentsBodyBench` with `CRATONVM_MH_IWA_JDK_BODY=1
  CRATONVM_MH_FOREIGN_AS_TYPE_CACHE_READ=1`, adding each new switch in turn, JIT and `--nojit`,
  interleaved medians. Expected: `static-int`, `static-ref`, `virtual`, `bound` and `list` lose the
  interpreted body (both switches) and the `asSpreader` chain (the memo); `varargs-control` stays
  flat. The census (`CRATONVM_DBG=mh-finder-census`, `reentry.iwa-body`) should drop to the two
  hidden `invokeBasic` re-entries plus `invokeExact_MT`. For the `vm_exec` part:
  `L7W115MethodIndexMemo` and the same bench with `CRATONVM_EXECUTE_METHOD_INDEX_MEMO=1`.

## Wave 120 (interpreter round i6, lane L4)

**SWITCH-OFF**: `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH` (`VmConfig::mh_interp_basic_frame_push`,
`runtime_flag_on`, default OFF, performance; `--nojit` only). Not built or run by the lane.

### Where the rest of the cost is (by reading, after waves 114-116)

With the body's own steps native (`CRATONVM_MH_IWA_NATIVE_STEPS`) and the spreader filed
(`_SPREADER_MEMO`), every remaining step of a call is a native-to-Java re-entry:

1. the linker (`mh_linked_invoker` -> `linkTo*` -> `Invokers$Holder.invokeExact_MT`);
2. `invokeBasic` into the spread invoker's `LambdaForm$MH`;
3. `invokeBasic` into the `asType` adapter's form;
4. `invokeBasic` into `DirectMethodHandle$Holder.invokeStatic`;
5. `linkToStatic` into the target.

Under a JIT, L2's `CRATONVM_JIT_MH_COMPILED_LINK` / `_BY_ID` (wave 116) turn 2-5 into compiled calls
once the forms compile. Without a JIT, every one of them is the registered native's door re-entering
`interpreter::execute` (a Rust recursion with its prologue and epilogue), although the caller is
interpreted `LambdaForm` bytecode the dispatch loop could simply continue.

### Built: an interpreted `invokeBasic` pushes its entry's frame

`vm/src/runtime/interpreter/sig_poly_site.rs`:

* `try_hit_site` (the signature-polymorphic route cache's hit, reached once the site's first call
  recorded the `invokeBasic` native's route for the receiver class) asks the new
  `try_basic_frame_push` first, under the switch and without a JIT.
* The handle's `form.vmentry` is read exactly as the door's first arm reads it
  (`lang_invoke::mh_compiled_basic_vmentry`: not a carrier or downcall handle, the vmentry
  switches on). The entry is decoded by `linkTo*`'s own memo (`mh_compiled_member`) and must be:
  * `REF_invokeStatic`;
  * with an initialized owner (`bind_by_id`, found by class id, so hidden spun `LambdaForm$MH`
    classes work);
  * static, not synchronized, with a `Code` attribute and no registered native in front of it;
  * typed as the handle, then the site's values, in the same basic types, returning the site's
    basic type.
* The method's frame template (`CachedBytecodeMethod`, built as `populate_invoke_cache` builds one)
  is memoised per thread in `BASIC_ENTRY_MEMO`, keyed by VM, address epoch, redefinition count, the
  `MemberName`'s address and the site descriptor's hash. Declines are memoised too.
* Then the stack-overflow check as `dispatch_static`, the site's arguments popped as `run_route`
  pops them, `install_cached_frame`, and `FramePushed`. Anything else declines to the native,
  with nothing popped.

Steps 2-4 above then cost a frame push each instead of a re-entry. The frames are the same
methods the door's re-entry pushed, so walks, traces and exceptions do not change; only the native
frame between them is gone (HotSpot has none either). Steps 1 and 5 stay re-entries (`invokeExact`
through the linker door; `linkToStatic` is an `invokestatic`, not this cache's `invokevirtual`).
The next cut is the same push for `linkTo*` from `$Holder` bytecode.

Why `--nojit` only: under a JIT the forms compile through the door's `execute` entry, whose
counter a pushed frame would not bump, and compiled callers link them themselves (L2).

### Probe and A/B

* Behaviour, `--nojit`, both modes, switch off vs on: `L4W109IwaHiddenFrames` (with
  `CRATONVM_MH_IWA_JDK_BODY=1`), `L4W116IwaNativeSteps`, `L3W104WalkOptionsThroughHandles`,
  `L4W114AsTypeCacheRead`, `L4W117DirectHiddenLatch`, `L4W54RealJdkHandleArgs`,
  `L4W55LinkToFromBytecode`. Every line must be the same in both arms.
* A/B, `--nojit`, interleaved medians:
  * `L4W109InvokeWithArgumentsBodyBench` with `CRATONVM_MH_IWA_JDK_BODY=1
    CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1`, adding this switch;
  * `L4W102DirectMethodWarmBench`;
  * `L4W95FieldHandleWarm` (bound handles reinvoke through `invokeBasic`).

  Expected: the bodies lose three re-entries each.

### Wave 120 follow-up (lane L4)

* **Statics ratchet.** The memo is no longer a `thread_local!`. It was `BASIC_ENTRY_MEMO`, one
  more `static` declaration line in `vm/src`, which `vm/tests/per_vm_state_statics_ratchet.rs`
  counts. It is now a field of the thread's own route table (`SigPolySiteCache::basic_entries`,
  on the `JvmThread`, so it belongs to one VM). It is allocated on first use (32 lines) and keyed
  by address epoch, redefinition count, the entry `MemberName`'s address and a hash of the call
  name and site descriptor. Wave 120 adds no static, so the ratchet's baseline does not move.
* **`linkToStatic` from `LambdaForm` bytecode is a frame push too** (same switch, same `--nojit`
  gate). `sig_poly_site::link_to_static_frame_push`, called from `try_stackless_invoke`
  (`vm/src/runtime/interpreter/invoke.rs`, a small anchored edit in front of the erased
  `MethodHandle` / `linkTo*` arm), pushes the trailing `MemberName`'s method with the arguments
  minus the `MemberName`, where `native_method_handle_link_to` re-entered `execute`. The
  admission is the `invokeBasic` one through the shared `entry_method_memoized` /
  `entry_method_uncached` (lead 0, trail 1): `REF_invokeStatic`, owner initialized, static, not
  synchronized, `Code`, no registered native in front of it, basic types agree. Step 5 of the
  Wave 120 list is now a push. Only the linker step (1) still re-enters per call.
  `linkToVirtual` / `linkToInterface` / `linkToSpecial` are not taken (receiver selection and the
  null checks belong to the native).
* Probes and A/B: as in "Wave 120" above. Also `L4W55LinkToFromBytecode`, `L4W102DirectMethodWarmBench`
  and `L3W58HandleClassInitFrames` (static-init barrier frames) under `--nojit` with the switch.

## Run, round i6 (run lane R, w124, P-cores, `--nojit`): both switches KEPT-OFF, page OPEN

- `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH` alone is a large loss on warm `invokeExact`:
  `static-getter-warm-exact` 7.87x, `bound-getter-warm-exact` 7.39x, `jdk-site-warm-exact` 3.88x.
- The body arm (`CRATONVM_MH_IWA_JDK_BODY`) against base is 9.5-14.2x on `static-int` /
  `static-ref` / `virtual`. The push removes 30-45% of that, but body plus push is still 4-8x base.

Both switches stay default OFF, and the flip pair is not made. The 35 us gap this page names is
still open: a fix must make the body arm cheaper than base before either switch can move. Details:
`docs/internal/reviews/interpreter-i6-run-results-20261224.md`, section i110-L4.

## Wave 130 (lane L4), 2026-10-19

**SWITCH-OFF; the page stays OPEN.** Not built or run by the lane. Two changes:

1. a fix inside `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, meant to make the push neutral outside
   the body arm;
2. a new default-off switch, `CRATONVM_MH_IWA_LINK_MEMO`, for the body arm.

The w124 absolute rows (`/c/cvt-i6/runs/ab-w124/i110l4_*`, `--jdk-only --nojit`) are the
reference below.

### 1. Why the push lost 3-8x on warm `invokeExact`

These are the costs per call, found by reading `sig_poly_site::try_basic_frame_push`'s
`invokeExact` arm. It is reached on every interpreted `invokeExact` whose route the site cache
holds.

* **The before-linker lanes were bypassed. This is the 7x.**
  * Rows: `static-getter-warm-exact` 313 -> 2463 ns, `bound-getter-warm-exact` 410 -> 3031,
    `static-warm-exact` 1193 -> 3549, `jdk-site-warm-exact` 1621 -> 6285.
  * The door answers a field accessor, a plain `DirectMethodHandle` and a `bindTo` of an
    accessor with one native lane (`mh_real_field_before_linker`, `mh_real_direct_before_linker`,
    `mh_real_bound_field_before_linker`, all default ON). For a `DirectMethodHandle`, that lane
    makes one re-entry, into the target.
  * The push asked `mh_compiled_linker_pair`. That function asks about the lanes only for a
    *user-class* site (`(LBox;)I`). A *shareable* site (`()I`, `(I)I`, `(Ljava/lang/String;)I`)
    with a filed `linker:` row went straight to the linker frame. The row is filed by the door's
    first call, which runs the linker before the lane's site memo is warm.
  * So every call interpreted `Invokers$Holder.invokeExact_MT`: `checkExactType`,
    `checkCustomized` and `maybeCustomize`, then `invokeBasic` (a push), then the
    `DirectMethodHandle$Holder` form, then `linkToStatic` / `Unsafe.get*`. That is five or six
    interpreted frames at ~350-400 ns each under `--nojit`, where the door made one native call.
  * The JIT's compiled-site rule is right for the JIT: under a JIT those frames compile, and the
    w103b A/B measured the lanes' handles slower through the compiled chain only at user sites.
    For the interpreter it is wrong at every site.
* **The site's shareability was re-parsed on every declining call (the 1.12-1.57x rows).**
  * Rows: `getter-warm-exact` 400 -> 620, `setter-warm-exact` 414 -> 651,
    `method-warm-exact` 1.21, `virtual-warm-*` 1.12-1.17. All of these are user-class sites, so
    the push declined, but only after `mh_compiled_linker_pair` had made its checks:
    * `object_num_fields`;
    * `is_downcall_handle`;
    * a by-name `form` read;
    * `bound_method_type_shareable`, which parses the descriptor into `String`s and makes a
      class-manager lookup per `java/` component.
  * The door then asked all of it again. That is about 220 ns a call. Nothing was memoised.
* **On a push, the pair row was looked up twice.**
  * `mh_compiled_linker_pair` ran before and after the template decode. Each ask makes a
    `format!` of the `linker:invokeExact:<site>` key, a `class_keyed` string and a probe of the
    VM's rooted table.
  * The second ask exists only because the decode may collect, and an `entry_memo` hit never
    does.
* There is no per-call allocation in the push itself. The frame template is memoised
  (`entry_memo`), and the push adds no safepoint and no frame listing.

**Change** (`CRATONVM_MH_INTERP_BASIC_FRAME_PUSH`, same switch, `--nojit` only):

* `lang_invoke::mh_interp_exact_link` is the interpreter's own question. It answers
  `DoorForGood` in three cases:
  * the handle's class is lane-served (`mh_user_site_lane_serves_handle`, by class id, a
    thread-local hit);
  * the site can hold no row with the user-site switch off;
  * the handle is a carrier, or the caller id is 0.

  Otherwise it answers `mh_compiled_linker_pair`'s pair, as `Pair` for a shareable site's rooted
  row or `PairOnce` for a user-site pair, or `Door` when no row is filed yet.
  `mh_compiled_linker_pair`, the JIT's question, is unchanged.
* `sig_poly_site::exact_link` remembers `DoorForGood` and `Pair` per (address epoch, site key
  hash, caller class id, handle class id). It keeps 16 lines in the same thin box as the entry
  memo (`BasicMemo`, so `SigPolySiteCache`'s frozen layout does not move).
  * A `Pair` hit asks the per-object half again (`mh_interp_exact_handle_admits`: not a carrier,
    not a downcall handle, `form` set).
  * `Door` and `PairOnce` are asked again on every call. `PairOnce` re-asks because the door's
    loader-ambiguity refusal is per call.
* The appendix is asked again after the template decode only when the address epoch moved.

Expected: the lane rows (`static-getter`, `bound-getter`, `getter`, `setter`, `ref-setter`,
`method`, `static-warm-exact`, `virtual-warm-*`, `jdk-site`) come back to base, within the cost of
one memo probe. Combinator handles (`dropArguments`, spreaders, adapters) at shareable sites still
push. Under the body arm, the push's `invokeBasic` / `linkToStatic` arms are unchanged.

### 2. What the body arm (`body3`) still pays per call

`body3` is `CRATONVM_MH_IWA_JDK_BODY` + `_NATIVE_STEPS` + `_SPREADER_MEMO`. Reference rows:
`static-int` 31.4 us (17.8 us with the push) against 2.2 us base. The same chain served
through the base door, as `bound` / `varargs-control` are, costs 22-34 us base. So the chain
itself is the cost, not anything specific to `invokeWithArguments`. By reading, largest first:

1. **The interpreted `LambdaForm` chain, about 20 interpreted invocations a call (~8 us at
   ~400 ns each under `--nojit`).**
   * `invokeExact_MT`, then `checkExactType`, `checkCustomized` and `maybeCustomize`. The memo
     resets the spreader's count, so `maybeCustomize` runs on every call.
   * The spread form, then `checkSpreadArgument` and the `aaload`s.
   * The generic invoker form, then `checkGenericType` (the `asType` `Intrinsic`'s same-type
     answer) and `checkCustomized`.
   * The `asType` adapter form, then the unboxing helper, the `DirectMethodHandle$Holder` form,
     `internalMemberName`, `linkToStatic`, the target, and `Integer.valueOf`.

   These frames are exactly what the body arm exists to show (HotSpot's hidden frames), so they
   cannot be skipped. Their price is the interpreter's per-invoke cost (L1 / L7), and the push
   already removed the re-entries between them.
2. **Re-entries: one per call is left** (`mh_linked_invoker` -> `native_method_handle_link_to`
   -> `interpreter::execute` for `invokeExact_MT`). Without the push there are four more, which
   the push removes (31.4 -> 17.8 us).
3. **The door's per-call setup around that re-entry, about 1-3 us (the cut).** This work runs
   on every call and answers the same thing each time:
   * `mh_iwa_spreader`: `class_id_by_name("java/lang/invoke/MethodHandle")`, a `format!` and
     `class_keyed` key, a table probe;
   * `mh_linked_invoker`'s handle and site questions: `method_exists(MethodType.fromMethodDescriptorString)`,
     a linear scan of `MethodType`'s methods; a by-name `form` read; `mh_site_access_refusal`,
     which parses the site; the NPE check;
   * `bound_method_type_shareable(site)`: a descriptor parse into `String`s, two class lookups
     and two stub checks;
   * a second key `format!`, a second table probe, and the `Vec`s of `pin_mh_args` and
     `values`.
4. **Boxing and the varargs array.** These are the caller's, the same in both arms: the bench
   boxes `seven` once and javac builds the `Object[]` per call. The `Integer.valueOf` return box
   is part of item 1. The `asType` spin is not per call: `mh_real_iwa_adapted` memoises it per
   epoch, and `checkGenericType`'s `asType` takes the same-type answer.

**Built: `CRATONVM_MH_IWA_LINK_MEMO`.** It is `NativeFlags::mh_iwa_link_memo`, `truthy_word`,
default OFF, performance, with a `flag_groups` row and a `flag-surface` line. It is read only on
the native steps' path and only with `_SPREADER_MEMO`; it is off under the census and under
`dbg_mh_dispatch`. It lives in `lang_invoke.rs`:

* The thread-local `IWA_LINK_MEMO` (per VM by key, like `IWA_ADAPT_MEMO`) holds two
  four-line halves.
  * Spreader lines are keyed by (VM, address epoch, n). They are filled from
    `_SPREADER_MEMO`'s rooted row on a table hit, and `customizationCount` is still reset
    before every use.
  * Linker lines are keyed by (VM, address epoch, caller class). They hold the
    `linker:invokeExact:(MethodHandle,Object[])Object` row's `(MemberName, appendix)`, read
    after the door ran (`iwa_link_line_fill`). The caller is the door's `near`, computed in the
    same place, inside the reflective mark.
* `iwa_link_memo_call` is the door's pair hit, made from the line:
  * the door's `StackOverflowError` check;
  * the compiled-stack budget rebase;
  * the spreader and the arguments pinned;
  * the root refresh as `CRATONVM_MH_LINKER_HIT_SKIPS_ROOT_REFRESH` says;
  * `native_method_handle_link_to(spreader, adapter, array, appendix, member)`.
* The door's other per-call questions answer the same for this fixed site (see the function's
  doc):
  * `mh_site_access_refusal` never refuses `MethodHandle` / `Object[]` / `Object`;
  * the NPE needs a null first argument, and the adapter never is null;
  * the spreader's `form` is final.

Expected: `static-int` / `static-ref` / `virtual` / `list` lower by the setup in item 3, in both
`body4` and `body4push`. `bound` and `varargs-control` stay flat, since they do not reach the
native steps. The switch does not close the gap: item 1 stays, so `CRATONVM_MH_IWA_JDK_BODY` still
cannot become cheaper than base. The honest floor of the body arm under `--nojit` is the JDK chain
interpreted, which is what `bound` / `varargs-control` already pay in base (22-34 us).

### Probe

`tools/probes/interp/L4/L4W130ExactPushAndLinkMemo.java`. Its `HOTSPOT_EXPECTED` block comes from
25.0.3, and default flags and `-Xint` print the same. It has two parts:

* Part A: the lane handles, warm and after `System.gc()`; a `dropArguments` combinator, which
  still pushes, at a shareable site, warm and after a GC; a null first argument on a bound
  species; `WrongMethodTypeException`; a throwing target and its trace.
* Part B: arities 0-3; two caller classes alternating; after a GC; the `List` overload; a
  throwing target; recursion through `invokeWithArguments` into `StackOverflowError`, then
  calls after it.

### For the run lane

Correctness, `--nojit` in both modes, plus one JIT run per mode for the last arm. Every probe
must print the same output in every arm as in arm 1 (or as its `HOTSPOT_EXPECTED` block, except
for the known hidden-row gaps already listed in "Wave 120"). The arms:

1. no switch;
2. `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`;
3. `B4="CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1 CRATONVM_MH_IWA_LINK_MEMO=1"`;
4. `$B4` + `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`.

The probes:

* `L4W130ExactPushAndLinkMemo`;
* the Wave 120 list: `L4W109IwaHiddenFrames`, `L4W116IwaNativeSteps`,
  `L3W104WalkOptionsThroughHandles`, `L4W114AsTypeCacheRead`, `L4W117DirectHiddenLatch`,
  `L4W54RealJdkHandleArgs`, `L4W55LinkToFromBytecode`, `L3W58HandleClassInitFrames`;
* `L4W84InvokeWithArgumentsJumbo`, `L4W95IwaVarargsSuper`, `L4W98SpreaderConversion`;
* the checksum lines of `L4W95FieldHandleWarm`, `L4W102DirectMethodWarmBench` and
  `L4W109InvokeWithArgumentsBodyBench`.

Run the A/B on P-cores (`start /affinity FFFF`), `--nojit`, `--jdk-only` and `--compatible`,
five interleaved reps of each arm in turn, and take medians. The arms:

* `base`: no switch;
* `push`: `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`;
* `body3`: `CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1 CRATONVM_MH_IWA_SPREADER_MEMO=1`;
* `body4`: `body3` + `CRATONVM_MH_IWA_LINK_MEMO=1`;
* `body3push`, `body4push`: each of those + `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH=1`.

The commands:

```text
cratonvm $mode --nojit -cp . L4W95FieldHandleWarm time          # base vs push
cratonvm $mode --nojit -cp . L4W102DirectMethodWarmBench 5      # base vs push
cratonvm $mode --nojit -cp . L4W109InvokeWithArgumentsBodyBench # all six arms
```

What each result decides:

* **`push/base` on every `L4W95` / `L4W102` row is the push fix.** The w124 losses were 7.87,
  7.39, 3.88, 2.97, 1.57, 1.55, 1.53, 1.21 and 1.12-1.17. Pass: each row at or below 1.05.
  If `static-getter` / `bound-getter` / `jdk-site` still lose, the lane check is not reached: run
  with `CRATONVM_DBG=mh-finder-census` and see whether the door counts `pair-hit` rows for those
  sites.
* **`body4/body3` and `body4push/body3push` on `static-int`, `static-ref`, `virtual` and `list`
  are the link memo.** Expected below 1, by about 1-3 us. `bound` and `varargs-control` should be
  about 1.0.
* **`body4push/base` is where the body arm stands.** Expected still well above 1 (item 1), so
  `CRATONVM_MH_IWA_JDK_BODY` stays off.
* **`push/base` on `L4W109` (no body).** `bound` and `varargs-control` run the chain through the
  door. Watch them for a regression from the lane check, since a `Species_LL` spreader declines
  the push now. w124 had 0.61-1.10.

Pass for a flip of `_INTERP_BASIC_FRAME_PUSH`: no `push/base` row above 1.05 in either mode, and
every behaviour arm SAME. `_IWA_LINK_MEMO` can only move together with `_IWA_JDK_BODY`, which
stays off.

## Wave 132 (lane L9 review), 2026-10-19

Review of wave 130's `exact_link` memo (`sig_poly_site.rs`) and
`CRATONVM_MH_IWA_LINK_MEMO` (`lang_invoke.rs`). No defect found, so no edit.
* The memo key holds raw addresses, and both memos are keyed by the address
  epoch (`HeapRealm::address_epoch`, which also moves at G1 cleanup and at the
  Generational sweep slices). The member and appendix are rooted rows. So a
  stale or freed address is never read back. Class unloading needs a
  collection, which moves the epoch, so a reused class id cannot hit. The
  linker pair does not depend on the caller's redefinition.
* `IWA_LINK_MEMO` is a `thread_local!` in `native-builtins`, which no statics
  ratchet scans (the ratchets cover `vm/src`, `classloading/src` and
  `jit/src`). Its lines are keyed by `vm_identity`, which is never reissued.
  It is a perf cache, not compatibility state, so the per-VM rule holds. It
  has the same shape as `IWA_ADAPT_MEMO`.
* Both switches are default OFF, and their OFF arms are unchanged. Under
  `CRATONVM_MH_INTERP_BASIC_FRAME_PUSH` itself, `mh_interp_exact_link` now
  keeps the door for handles a before-linker lane serves.
* Perf-only caveat: `DoorForGood` and `Pair` are filed per handle CLASS, but
  `mh_user_site_lane_serves_handle` looks at the handle object (a `bindTo` of
  an accessor). Within one epoch, two handles of one `BoundMethodHandle`
  species at one site share the first one's decision. The result is correct
  either way, because the linker checks the type, but one of them may take the
  slower route until the next collection.

## Wave 132 (lane L4), 2026-10-19

**SWITCH-OFF; the page stays OPEN.** Not built or run by the lane.

### The largest cut left in item 1 of "What the body arm still pays"

The frames of the chain must stay, because they are what the body arm is for. But eight of its
roughly twenty interpreted invocations leave no frame behind. They are the invoker forms' guard
calls, and they return before the call they guard:

| call site (per `invokeWithArguments`) | JDK 25 body | interpreted invocations |
| --- | --- | --- |
| `invokeExact_MT`: `checkExactType(spreader, mt)` | `mh.type()`, compare | 2 |
| `invokeExact_MT`: `checkCustomized(spreader)` | `isCompileConstant`, `form.customized`, `maybeCustomize` -> `MethodHandle.maybeCustomize` (the spreader's count is reset per use, as HotSpot's fresh spreader starts at 0, so this runs on every call) | 4 |
| spread form: `checkCustomized(adapter)` | `isCompileConstant`, `form.customized` (and `maybeCustomize` for the first 127 calls) | 2 (4) |

That is about 3 us a call at ~400 ns an invocation under `--nojit`, the largest group of item 1
that the walk cannot see. The other leaf helpers stay: `checkSpreadArgument`, `checkGenericType`
(already the `asType` `Intrinsic`'s same-type answer), the unboxing helper and
`internalMemberName`.

### Built: `CRATONVM_MH_INVOKER_CHECKS_INTRINSIC`

The flag is `NativeFlags::mh_invoker_checks_intrinsic`, `truthy_word`: default OFF, performance.
It has a `flag_groups` token, `mh-invoker-checks-intrinsic`, and a `flag-surface` line. It is read
at registration. The rows register only in a VM without a JIT
(`interpreted_intrinsic_registers_row`, row `invoker-checks`), as `MethodType.makeImpl`'s memo does
since wave 75. Both methods are `@ForceInline`, and a native-backed method is never compiled.

`native-builtins/src/lang_invoke.rs` has two reviewed `Intrinsic`s over JDK bytecode. Each answers
only the plain case itself:

* `Invokers.checkCustomized(MethodHandle)V` (`invokers_check_customized_intrinsic`):
  * a customized form: nothing to do;
  * the default threshold with a count below 127: the count is raised by one;
  * anything else runs the JDK's own body: a due customization, a non-default
    `CUSTOMIZE_THRESHOLD`, an unreadable `form` (a carrier), or null.
* `Invokers.checkExactType(MethodHandle, MethodType)V` (`invokers_check_exact_type_intrinsic`):
  * the handle's `type` field identical to the expected (interned) type: nothing to do;
  * anything else runs the body. That covers another type, so the body raises
    `WrongMethodTypeException` with its own `newWrongMethodTypeException` / `checkExactType`
    frames. It also covers a null, and a carrier whose `type` is unset (its `type()` native fills
    it).

The body is `invokers_check_jdk_body`, which uses `invoke_special_bytecode_only` under
`continuation_pin::transparent`. That adds a router census row, `base java/lang/invoke/Invokers`,
and `FROZEN` is updated (noted on the i74-L1 page).

These rows serve every interpreted `invokeExact_MT` / `invoke_MT` linker and invoker form in a
`--nojit` VM, not only `invokeWithArguments`. A debugger breakpoint inside the two methods does
not fire, as for every `Intrinsic`.

Expected effect:

* body arms: ~2.5-3 us less a call, a native crossing each in place of 2-4 interpreted
  invocations;
* `bound` and `varargs-control`, which run the chain through the door in base: a similar gain;
* the lane-served rows of `L4W102` / `L4W95`, which never reach the linker: flat.

This does not close the page. Under `--nojit` the body arm stays several times base (item 1's
frames remain).

### For the run lane

Behaviour, `--nojit`, `--jdk-only` and `--compatible`, with `I=CRATONVM_MH_INVOKER_CHECKS_INTRINSIC=1`
against without it. Every probe must print the same output in both arms:

* `L4W132IwaCustomization` (with `--add-opens java.base/java.lang.invoke=ALL-UNNAMED`; the counts
  are now written by the `Intrinsic` on the linker paths);
* `L4W130ExactPushAndLinkMemo` (its `WrongMethodTypeException` row and trace);
* `L4W116IwaNativeSteps`, `L4W117DirectHiddenLatch`, `L4W109IwaHiddenFrames` (with
  `CRATONVM_MH_IWA_JDK_BODY=1`);
* `L3W46InvokeFramesInWalks`, `L3W115SpunIwaWalkFrames`, `L3W104WalkOptionsThroughHandles`;
* `L4W54RealJdkHandleArgs`, `L4W55LinkToFromBytecode`, `L4W103CompiledUserSiteLink`.

Also run one JIT pass of `L4W130ExactPushAndLinkMemo` with `$I`: same output, and no timing
change. The rows do not register there: `interpreted_intrinsic_registers_row` is false with a JIT
unless `CRATONVM_INTRINSIC_BODY_COMPILES` or its row list names `invoker-checks`.

A/B on P-cores, `--nojit`, five interleaved reps, medians:

```text
cratonvm $mode --nojit -cp . L4W109InvokeWithArgumentsBodyBench   # arms: base, base+$I, body4, body4+$I
cratonvm $mode --nojit -cp . L4W102DirectMethodWarmBench 5         # base vs base+$I
cratonvm $mode --nojit -cp . L4W95FieldHandleWarm time             # base vs base+$I
```

Here `body4` is `CRATONVM_MH_IWA_JDK_BODY=1 CRATONVM_MH_IWA_NATIVE_STEPS=1
CRATONVM_MH_IWA_SPREADER_MEMO=1 CRATONVM_MH_IWA_LINK_MEMO=1`.

Pass for a flip (default ON in no-JIT VMs):

* `body4+$I / body4` below 0.90 on `static-int`, `static-ref`, `virtual` and `list`;
* `base+$I / base` at or below 1.02 on every row of all three benches;
* `bound` and `varargs-control` below 1;
* every behaviour arm SAME.

If the field reads cost what the memory note says (~300 ns per small `Intrinsic`), the
`checkExactType` half may be flat. Report each row so the two methods can be split.
