# Transformers are handed a `null` ProtectionDomain, so JaCoCo's default filter instruments no class

**Status: open, narrowed (wave 29: `--compatible` class-path classes fixed; user-loader defines in `--compatible` remain) — filed 2026-09-29 by interpreter round i1 wave 28, lane L3
(found reviewing the transformer-argument plumbing while fixing the `loader`
argument, `L3/L3W28RetransformLoader`). Narrowed by the wave-28
orchestrator (below): `--jdk-only` retransformation, and load time for classes
a loader defines through `defineClass1/2`; then, after the wave, load time for
the class-path classes the VM loads itself, so JaCoCo's default filter no
longer skips a plain `-cp` application's classes under `--jdk-only`. (The real
agent still fails earlier, in `premain`, on `dev` too:
`i28-L5-an-agent-jars-classes-are-split-between-two-loader-identities`.) Open:
`Lookup.defineClass` (fixed in wave 29 for `--jdk-only`, lane L5; see below), and `--compatible`.**

## Progress (after wave 28) — class-path classes at load time

`--jdk-only`, a class the application loader defines from the class path:

* `pre_transform_for_load` (`vm/src/runtime/instrument.rs`) asks
  `ClassManager::find_class_code_source` for the code source the definer will
  record (the lookup `define_class_with_options` makes), and
  `lang_class::app_class_path_domain` builds the domain from it: the
  domain-building tail of `protection_domain_from_code_source` is now
  `build_code_source_domain`, which takes the code base, the signer
  certificates and the loader (the app loader here, the mirror's
  `getClassLoader()` there).
* One domain per code base, as `SecureClassLoader`'s `pdcache` keeps one per
  `CodeSource`: the per-VM `app_code_source_domains` table beside the loader
  singletons (`native-builtins/src/classloader.rs`), GC-rooted and remapped
  with them and cleared with the VM, or when the app loader singleton is
  rebuilt. `protection_domain_from_code_source` answers from it for a
  class-path class of the application loader, so `getProtectionDomain()` is
  the object the transformer saw, and every class of one jar shares it, as on
  HotSpot. The per-class `file:/runtime-defined/<name>.class` stand-in of a
  generated class is not cached (it would grow with every generated class).
* `--compatible` and every other class keep a fresh domain per call.

`L3/L3W28TransformerDomain` now also prints whether the load-time domain is
the class's own and whether two classes of the jar share one; HotSpot prints
`true` for both.

## Progress (wave 28) — orchestrator

`--jdk-only` hands the transformers HotSpot's domain on two paths:

* **Load time.** The `defineClass1` / `defineClass2` natives
  (`native-builtins/src/lang_system.rs`, the ones that already pin the domain
  to install it in the mirror) put it in the new
  `DefineClassFull::protection_domain`; `VmExec::define_class_full` passes it
  to `run_load_time_transform_chain`, which pins it across its loader lookup;
  `run_chain_over_bytes` pins it with the mirror and the loader and passes it
  as the `protectionDomain` argument.
* **Retransform / redefine.** `run_chain_over_bytes` asks
  `lang_class::protection_domain_of_mirror` (what `getProtectionDomain0`
  answers: the installed domain, `null` for a bootstrap class) after the
  mirror is pinned.

Probe: `tools/probes/interp/L3/L3W28TransformerDomain.java` (agent jar).
HotSpot 25 prints `load pd=location` / `retransform pd=location`; `--jdk-only`
prints `load pd=null` / `retransform pd=location` (host run, wave 28: its
`Target` is a class-path class, item 1 below); `--compatible` prints `null`
twice.

### What remains

1. **Fixed after wave 28** (above). `--jdk-only`, load time, class-path
   classes — the one that matters for JaCoCo on a plain `-cp` application. The VM serves the application class
   path itself: `pre_transform_for_load` (`vm/src/runtime/instrument.rs`) finds
   the bytes through `ClassManager::find_class_bytes_for_transform` and runs
   the chain before any class id exists, so `lang_class::protection_domain_of_mirror`
   cannot be asked. Fix: have the lookup return the entry's code base, and
   factor the domain-building tail of `lang_class::protection_domain_from_code_source`
   into a function keyed by code base (with a per-VM cache per code source, as
   `SecureClassLoader`'s `pdcache` makes one domain per `CodeSource`), then
   pass that domain here. The class's later `getProtectionDomain()` must
   answer the same object.
2. `--jdk-only`, `Lookup.defineClass` of a non-hidden class
   (`defineClass0`): the lookup class's domain is not passed yet.
3. `--compatible`: still `null` on both paths (its define natives are the
   `classloader.rs` ones and it installs no domain in the mirror; the change
   would need a census, as every `--compatible` change does).

## Evidence

* `vm/src/runtime/instrument.rs`, `run_chain_over_bytes`: every transformer
  call passes `Value::Object(None)` as `protectionDomain` ("null is
  spec-legal"), on all three paths -- the load-time hook
  (`run_load_time_transform_chain`, from `pre_transform_for_load` and from
  `define_class_full` in `vm/src/vm/vm_exec.rs`), `retransformClasses0` and
  `redefineClasses0`.
* JaCoCo's agent (`CoverageTransformer.filter`) skips a class with a non-null
  loader when `!inclNoLocationClasses && !hasSourceLocation(protectionDomain)`,
  and `inclnolocationclasses` defaults to `false`. With a `null` domain every
  application class is filtered out: a `-javaagent:jacocoagent.jar` run on
  CratonVM instruments nothing and reports 0% coverage, without an error.
* HotSpot 25, a load-time and a retransform-capable transformer offered an
  application class (`-javaagent` jar, class on `-cp`): `load pd=location`,
  `retransform pd=location` (the domain has a `CodeSource` with the jar's
  location). CratonVM: `null` for both (read from the code).

## What HotSpot does

`JvmtiClassFileLoadHookPoster` passes the protection domain the class is
being defined with (the `defineClass` argument; for the built-in loaders the
domain `SecureClassLoader.getProtectionDomain(CodeSource)` built for the jar),
and on a redefinition or retransformation the class's own
(`the_class->protection_domain()`); `null` only for a class defined without
one (the bootstrap loader's).

## Recommended fix

* Retransform / redefine: the class's domain. `Class.getProtectionDomain()`
  answers a synthetic all-permission domain for a class that has none, so
  read the mirror's own domain instead (the native behind
  `getProtectionDomain0`), and pass `null` when there is none. Plumb it as a
  `domain: Option<ObjectRef>` argument of `run_chain_over_bytes`, pinned with
  the mirror and the loader (`defining_loader_of_mirror` is where to ask).
* Load time, `define_class_full`: the `ProtectionDomain` the `defineClass*`
  native received; `DefineClassOptions` carries only the code source's URL
  and certificates today, so the natives (`native-builtins`, lane L4/L5) must
  hand the object through.
* Load time, built-in delegation (`pre_transform_for_load`): the domain the
  application loader would build for the class's code source; CratonVM's
  `--compatible` app loader builds none, so a domain must be minted from the
  `CodeSource` the define records (`ClassManager` `code_source`).

Verify with a probe shaped like the check above (a transformer printing
`pd=null|no-codesource|location` at load and at retransform), in all four
modes.

## Progress (wave 29) — lane L5 (item 2, the define side)

`--jdk-only`, `Lookup.defineClass` of a non-hidden class, both doors it can
arrive through:

* `native-builtins/src/lookup_define.rs` `lk_define_class_b` (the registered
  `Lookup.defineClass` bridge) defined the class under the name `""`, and
  `VmExec::define_class_full` runs the load-time chain only for a named define,
  so such a class was **never offered** to a transformer at all (worse than a
  `null` domain). It now passes the class file's own name, and the lookup
  class's domain (`lang_class::protection_domain_of_mirror` of the lookup
  class's mirror; none for a bootstrap lookup class, as
  `Lookup.ClassDefiner.defineClass` passes `null` there), pinned across the
  define, as `DefineClassFull::protection_domain`, and installs it in the new
  class's mirror (`protectionDomain`), as `defineClass1` does.
* `native-builtins/src/lang_system.rs` `defineClass0` (the real
  `Lookup.defineClass` bytecode's native) already pinned the domain and
  installed it in the mirror, but did not pass it to the chain: it now does,
  for a non-hidden class.

Probe `tools/probes/interp/L5/L5W29LookupDefineDomain.java` (agent jar).
HotSpot 25:

```
defined L5W29LookupDefineDomain$Gen value=3
load pd=location
load pd is the lookup class's=true
class pd is the lookup class's=true
```

`--compatible` is unchanged (`load pd=not offered`, `false`, `false` expected):
item 3.

### Item 3 (`--compatible`): the census the orchestrator should run

The change a `--compatible` fix would make is observable only to a
`ClassFileTransformer`, so the census is the agent workloads, not the plain
suite:

1. Build with a counter: in `run_chain_over_bytes` (`vm/src/runtime/instrument.rs`,
   lane L3), count under `--compatible` each load-time call whose
   `protectionDomain` argument is `null` for a class whose loader is not the
   bootstrap loader, and print the per-VM total at exit under
   `CRATONVM_DBG=access` (the existing census line's arm is the place).
2. Run, `--compatible`, JIT and `--nojit`: `agentprobes.sh`, Mockito 5.23 inline
   mocks (`mock/run.sh`), and the JaCoCo run (`jac.sh`, which also needs
   `i28-L5-an-agent-jars-classes-are-split-between-two-loader-identities`'s
   wave-29 fix).
3. Decision rule: a non-zero count on a workload whose output differs from
   HotSpot because of it (JaCoCo's 0% coverage is the known one) is the case
   for porting the `--jdk-only` behaviour (the three define natives' domain,
   and `pre_transform_for_load`'s `app_class_path_domain`) to `--compatible`;
   a count with no output difference on every workload keeps `--compatible`
   as it is.

## Progress (wave 29) — orchestrator (item 3, class-path half)

`--compatible` now hands the load-time transformers the application loader's
per-code-base domain for a class-path class, and hands a retransformation the
class's own domain, as `--jdk-only` has done since wave 28. `getProtectionDomain()`
answers the same object. `native-builtins/src/lang_class.rs`
`app_class_path_domain` and `protection_domain_from_code_source`, and
`vm/src/runtime/instrument.rs`, lost their `is_jdk_only` gates. This is a
genuine bug, not a census change: HotSpot passes the domain, and without it
JaCoCo's default filter skipped every class, so a `--compatible` coverage run
recorded nothing.

Host run of the wave-29 head:
* `L3/L3W28TransformerDomain` matches HotSpot in all four modes; `--compatible`
  differed before.
* JaCoCo 0.8.15 under `--compatible`: `f=1`, and the `.exec` file now names
  the application class.

**What remains:** `--compatible`, a class a USER loader defines through
`defineClass` or `Lookup.defineClass`. Its define natives (`classloader.rs`)
install no domain in the mirror, and the load-time chain still gets `null`
there (`L5/L5W29LookupDefineDomain`, `--compatible` rows).

## Wave 39 note — lane L3: not attempted (not local)

The brief allowed a fix only if local and shown by a probe. It is not local:
the `--compatible` user-loader defines are five natives in
`native-builtins/src/classloader.rs` (`cl_define_class_basic`, `_pd`, `_bb`,
`cl_define_class1` / `2` / `0`, `lk_define_class`; lane L5's file), and each
would have to install the `ProtectionDomain` argument in the new mirror and
hand it to the chain (`DefineClassFull::protection_domain`), which changes
what `getProtectionDomain()` answers for every user-loader class under
`--compatible` -- the census item 3 above describes, not a transformer-only
change. The `--jdk-only` natives (`lang_system.rs` `defineClass1/2/0`,
`lookup_define.rs`) are the pattern to copy once the census says so.
