# Proposal: run the JDK's own `InstrumentationImpl`, natives only for its leaves

**Status: proposal — filed 2026-10-06 by interpreter round i1 wave 42, lane
L3. Not implemented.**

## Where it stands

The `InstrumentationImpl` the VM hands an agent is a bare allocation
(`agent_loader::build_instrumentation_mirror`; the self-attach path calls a
registered no-op `<init>`). Its `TransformerManager`s never exist, so every
public method whose Java body would touch them is shadowed by a registered
`Bridge` native in `instrument::register_instrumentation_natives`:
`addTransformer` (both), `removeTransformer`, `isModifiableClass`,
`getObjectSize`, `getAllLoadedClasses`, `retransformClasses`,
`redefineClasses`, `isRetransformClassesSupported`,
`isRedefineClassesSupported`, `isNativeMethodPrefixSupported`. The
transformers live in a per-VM Rust chain (`TransformerChains`), with its own
GC root scan and remap, and `run_chain_over_bytes` re-implements
`TransformerManager.transform` and `InstrumentationImpl.transform`.

Each copy has drifted from the JDK's Java, and each drift is a divergence a
probe found:

* wave 26: the offer order (non-retransformable first) —
  `L3W26LoadHookFirstTouch`;
* wave 42: every `null` check and every capability check of the public
  methods (`L3W42InstrumentArgumentChecks`, `L3W42InstrumentCapabilities`),
  and the `transform(Module, ...)` form with its module
  (`L3W42TransformerModuleForm`).

Also: the registered `<init>` has the descriptor `(JLjava/lang/String;ZZ)V`;
JDK 25's constructor is `InstrumentationImpl(long nativeAgent, boolean
environmentSupportsRedefineClasses, boolean
environmentSupportsNativeMethodPrefix, boolean printWarning)`, i.e.
`(JZZZ)V` (read in `java.instrument/sun/instrument/InstrumentationImpl.java`
of the JDK 25 sources). `run_self_attach`'s `ctx.invoke` of the old
descriptor reaches the registered no-op or nothing; either way nothing is
constructed.

## The proposal

1. Construct the object with the JDK 25 constructor: `nativeAgent` a per-VM
   agent handle (an index into a per-VM agent table holding the manifest's
   capabilities), `printWarning` false for `-javaagent:` and as HotSpot
   decides for a dynamic load.
2. Unregister the public-method shadows; keep natives only for the
   `ACC_NATIVE` leaves (`redefineClasses0`, `retransformClasses0`,
   `getAllLoadedClasses0`, `getInitiatedClasses0`, `getObjectSize0`,
   `isModifiableClass0`, `isRetransformClassesSupported0` (the handle's
   capability), `appendToClassLoaderSearch0`, `setHasTransformers`,
   `setHasRetransformableTransformers` (which arm the load hook),
   `setNativeMethodPrefixes`).
3. The chain walk becomes two calls of
   `InstrumentationImpl.transform(Module, ClassLoader, String, Class,
   ProtectionDomain, byte[], boolean isRetransformer)`: `false` then `true`,
   as libinstrument's two JVMTI environments do; the retransformation base
   is the answer of the first call.
4. Retire `TransformerChains`, `scan_transformer_roots` /
   `remap_transformer_refs` (the transformers are ordinary heap objects
   reachable from the `InstrumentationImpl`, which the agent table roots),
   and the wave-42 check copies.

## What to measure first

* `run_transformer_chain`'s comment says calling
  `InstrumentationImpl.transform` "panics deep inside the JDK 25 transformer
  pipeline (length-6 array indexed at 6)". That was before the invoke doors
  of the i1 round; re-run it (a probe with two transformers) before
  building anything.
* The Mockito special case (`add_instrumentation_transformer` keeps only the
  first `InlineBytecodeGenerator`) must move to where the second copy is
  registered, or be re-justified: on HotSpot both copies register.
* The `--jdk-only` shadow census: the eleven public-method `Bridge`s stop
  standing over bytecode (§1.4), which is the point of the mode.

## Why

HotSpot's checks, messages, order and exception handling come from the
JDK's own code instead of from copies, and the four divergences above could
not have happened. The shadows are `Bridge`s over concrete bytecode, which
`--jdk-only` exists to remove.

## Progress (wave 43) — lane L3

Not built. Of the two defect pages the lane section tied to it:

* `i42-L3-a-load-time-transform-that-returns-a-bad-class-file-is-dropped` was
  best fixed locally, and was: the defect was in the VM's load hook
  (`instrument::pre_transform_for_load` refused the chain's answer), which
  this proposal keeps -- item 3 replaces only the chain walk, not the hook
  that stages its answer
  (`docs/internal/fixed-bugs/interpreter-L3-a-load-time-transform-that-returns-a-bad-class-file-is-dropped-FIXED-20261007.md`).
* `i42-L3-native-method-prefix-is-never-supported` is not closed by it:
  `InstrumentationImpl.setNativeMethodPrefix` ends in the VM's
  `setNativeMethodPrefixes`, and the missing part is the VM's native linking
  (the page's wave-43 Progress names where).

What is left is the whole proposal, unchanged: items 1-4, and "What to measure
first" (the two-transformer run of `InstrumentationImpl.transform`, the
Mockito special case, the `--jdk-only` shadow census). The agent-jar probes
(Mockito, JaCoCo, every `tools/probes/interp/L3/*` agent probe) in all four
modes are the gate for it, which a lane that cannot run them should not
attempt blind.
