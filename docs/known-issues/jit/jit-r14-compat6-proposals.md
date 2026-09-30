# JIT round 14 wave 6, lane compat6: proposals

Status: OPEN (proposal book; the owner triages)
Found by: round 14 wave 6 lane compat6

Ranked by benefit over risk.

## CA6-1: census the "thin wrapper" Bridges mechanically, and retire them as one class of change

`Array.newInstance` was a `Bridge` over a Java body that is one `invokestatic` of a registered native
plus `areturn`. That shape is the cheapest retirement there is: no state, no carrier, and the answer
cannot change because the native the body reaches is already live. Nobody knew the two rows were
Java (the census page listed the class as "mostly `ACC_NATIVE`") until a trace probe showed a frame.

* Benefit: finds every such row at once instead of by trace accident; each is a zero-risk step of the
  census ranking (`r13w7-shadow-...-census`), moving `bridge_shadows_bytecode` down.
* Cost: one `#[ignore]`d test in `native-builtins/tests/` that reads the JDK 25 `java.base` jmod
  (the classfile parser already exists), and for every `Bridge` row whose target has `Code` reports
  whether the body is `(load args) invoke* T; *return` with `T` a registered native (or an
  `ACC_NATIVE`). Output: a candidate list, not a gate.
* Risk: none (read-only). Each retirement still needs its own screen.
* First step: write the bytecode matcher over `Code` for the `java/lang/reflect/` and `java/lang/Class`
  rows only and compare against a hand-read of five of them.

## CA6-2: one body for `newInstance` and `newArray` in `--compatible`

`native_array_new_instance` (`native-builtins/src/lib.rs`) and `lang_system::native_array_new_array`
duplicate the checks, their order and the component resolution (the second also reads slot 1 of a
legacy mirror; the first does not). They agree today by two waves of care. With the `--jdk-only`
rows retired, the `--compatible` native could be `reflect_array_component_ok` plus a tail call to
`native_array_new_array`, as `native_array_new_instance_multi` already is for its sibling.

* Benefit: removes a drift pair (a future fix to one body silently missing the other).
* Cost: ~30 lines deleted in `lib.rs`; `vm/src/vm/tests.rs::reflect_array_new_instance` covers both
  halves already.
* Risk: low; `--compatible` behaviour must stay byte-identical (the slot-1 fallback only widens what
  resolves).
* First step: diff the two bodies line by line (done in wave 6: same order, same errors).

## CA6-3: a stand-in pair for the multi-dimensional overload in `--compatible`

Wave 6 gives `Array.newInstance(Class, int...)` its real frame under `--jdk-only` only. In
`--compatible` the served native still throws with the caller on top where HotSpot prints
`Array.multiNewArray(Native Method)`, `Array.newInstance(Array.java:...)`. Two `STANDIN_METHODS`
rows (`vm/src/runtime/stackwalker.rs`, next to wave 5's `Array` pair, same switch
`CRATONVM_THROWABLE_STANDIN_ARRAY_FRAMES`): `newInstance(Ljava/lang/Class;[I)Ljava/lang/Object;`
(entry) and `multiNewArray(Ljava/lang/Class;[I)Ljava/lang/Object;` (entry).

* Benefit: `R14Compat6ArrayNewInstance` `f2` / `f4` match HotSpot in `--compatible` too.
* Cost: two rows. Risk: low (the chain screen already admits the three throwables).
* First step: run the probe's `--compatible` arm to confirm the difference.

## CA6-4: an allocation fast path for `Array.newArray` with a known component

Under `--jdk-only`, compiled `Arrays.copyOf(original, n, newType)` / `ArrayList.toArray(T[])` now
reach `Array.newInstance` bytecode and then the `newArray` `Bridge` (one extra Java frame, then the
native funnel: mirror reverse-map lookup, name compare for primitives). HotSpot's C2 intrinsifies
`Array.newArray` (`inline_unsafe_newArray`): when the component mirror's klass is known it emits a
plain array allocation.

* Benefit: the reflective copy paths lose the native funnel; measurable on collection-heavy loads
  (`toArray(new T[0])` is ubiquitous).
* Cost: an IR node or a direct helper taking the mirror, resolving it through the per-VM mirror
  reverse map once per site (inline cache on the mirror), then the ordinary `anewarray` helper;
  the refusals (null, negative, `void`, 255 dims) stay on the native path.
* Risk: medium (a new allocation path: GC maps, OOM / length cap parity with
  `VM_MAX_ARRAY_LENGTH`).
* First step: a `CRATONVM_DBG_NATIVE_ENTRY` census on a Spring boot under `--jdk-only` to see whether
  `newArray` is in the top call sites at all before building anything.
