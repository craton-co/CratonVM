# Proposal: memoize `invoke_or_native`'s by-name resolution

**Status: open — filed 2026-09-25 by interpreter round i1 wave 13, lane L5.**

## Why

`vm/src/vm/vm_exec.rs::invoke_or_native` is the general by-name resolver: the
JIT's virtual and static tails (`jit_invoke_dispatch_body`), the MIC tail
(`jit_invoke_virtual_mic_body`), `bail_to_interpreter`, lambda dispatch and
the native-callback door all end there when no site cache served the call.
Every call re-derives, from STRINGS, facts that change only when a class is
defined, redefined or a native registered:

1. the registry probe `find_with_kind_and_id(effective_class, name, desc)`
   (a 128-bit hash over all three strings);
2. for a call no native answers: a class-manager read, a by-name
   `get_loaded_class_id(effective_class)` and `find_method` for the
   has-own-bytecode test, then (when the class does not declare it) a
   superclass walk with a `find_with_kind_and_id` per ancestor;
3. the tail: another class-manager read, another by-name lookup (or the
   receiver's `get_class`), `dispatch_lacks_class_file`, and then
   `invoke_by_class_id_shared` -> `invoke_on_class_shared`, which resolves
   the method a third time.

Wave 13 removed the cheapest duplicates (the redefine probe now runs only
after some class was redefined; the has-own-bytecode test and the walk share
one read; the receiver test and the tail share one read; the null-name
`getResource*` arm tests the name before taking the class manager), but the
shape — three string-keyed questions per call with stable answers — remains.
The two `DowncallHandle` arms also take a class-manager read and a
`get_class` on every `type()` / `invoke*` call with an object receiver that
reaches this door, to compare a class NAME.

## Design

A per-VM memo (on `SharedVm`, next to the other dispatch memos; never a
process global) keyed by `(effective_class, method_name, descriptor)` —
interned `Arc<str>` pointers where the caller has them, else the hash the
registry already computes — holding one verdict:

* `Native { callback, kind, id, arm }` for the four registry arms (the arm
  decides which census/dial door is noted);
* `Bytecode { class_id }` for the tail when the class is loaded, has a method
  table (`!dispatch_lacks_class_file`) and no native is registered anywhere on
  the walked chain;
* `Unknown` (not memoized) for everything else: receiver-sensitive arms
  (`AnnotationProxy`, `Proxy$Instance`, `DowncallHandle`, ClassLoader
  intercepts), a redefined process (`any_class_redefined`), a loader-ambiguous
  name, and every `--jdk-only` dial or `SyntheticStub` arbitration.

Validity: tag each entry with `class_name_generation()` (moves on every
definition that can change a name's answer; see `NameEpochs`) and the
registry's `generation()`; a mismatch re-resolves. The receiver-class
shortcut of the tail stays per call (it is the loader-identity fix and is one
`get_class`).

For `DowncallHandle`: learn the class id once per VM (as
`learn_system_class_id` does for `System`) and compare
`heap.class_id_of(receiver)` against it with no lock.

## Staged plan

1. Instrument: count, under `CRATONVM_DBG=dispatch-tally`, how many
   `invoke_or_native` calls end in each arm (native arm / walk hit / tail
   bytecode / `invoke_shared`), on `HibfixComposeProbe2`, `ReactorProbe` and
   `XferProbe`. Only if the tail-bytecode share is large is stage 2 worth it.
2. The `DowncallHandle` class-id compare (no memo needed; small).
3. The memo for the tail-bytecode verdict only (the arm with no policy
   questions), behind a default-on flag for A/B.
4. Extend to the native arms once stage 3's A/B is flat or better, keeping the
   census/capability/dial calls on every hit.

## Verify

Per stage: the vm lib tests (`invoke_or_native_*`,
`i13_l5_static_native_init_tests`), the core suite JIT on and off, the
jdk-only suite, and interleaved medians of the three probes above (in-JVM
timings swing, so interleave binaries).

## Risk

Medium: this door carries many special cases whose correctness depends on
being asked every call. Everything receiver- or policy-sensitive stays
un-memoized by construction; the risk is an arm added later that forgets to
mark itself `Unknown` — a test should enumerate the arms, as
`intercept_shape_agrees_with_the_arms_it_gates` does for the interpreter's
intercepts.

## Progress (wave 16)

Interpreter round i1 wave 16, lane L3:

- **Stage 1 landed (instrument).** `vm/src/vm/vm_exec.rs`
  `invoke_or_native_impl` records the arm each call ends in under the
  existing `CRATONVM_DBG_DISPATCH_TALLY` gate (no new flag): site labels
  `invoke_or_native.arm.native` (the registry hit on the dispatch class),
  `.arm.alias` (the array-name retry), `.arm.parent` (both superclass-walk
  arms), `.arm.tail-loaded` (the bytecode tail on a loaded class — the
  verdict stage 3 would memoize) and `.arm.tail-load` (the load-then-dispatch
  fallback). The receiver-keyed arms are not labelled: they are the arms the
  design keeps un-memoized. Unarmed, each label costs the tally's cached flag
  load. Summing the dump's rows per label gives the arm shares the decision
  rule needs; run `HibfixComposeProbe2`, `ReactorProbe`, `XferProbe` and
  `tools/probes/interp/L3/ByNameCallBench.java` with the tally armed.
- Stage 2's `DowncallHandle` class-manager read is gone for STATIC calls
  (the `ByNameKind` gate, see
  `i14-L5-proposal-by-name-routes-carry-the-invoke-kind-20260925.md`); a
  virtual `invoke*` call still pays it. The lock-free compare needs a per-VM
  learned class id on `ClassRealm` (as `system_class_id`), which is outside
  this lane's files.
- Not landed: the memo itself (stages 3-4). Its keying must be exact across
  loaders (key by the dispatch `ClassId` where the tail has one, never the
  bare name, which is ambiguous once two loaders define it), redefinition
  (`any_class_redefined` disables it, as the native-callback memo does),
  unloading (`ClassId`s are not reused, but a name's answer changes:
  `class_name_generation()`), native registration (the registry's
  generation) and a second VM (per-`SharedVm` storage). Stage 3 waits on the
  stage-1 numbers.
