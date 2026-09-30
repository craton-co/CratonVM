# Proposal: `Lookup.defineClass` / `defineHiddenClass` run the JDK's own Java checks, and CratonVM takes over only at the define

**Status: proposal — filed 2026-10-08 by interpreter round i1 wave 44, lane
L5. Not implemented; wave 45 (lane L5) read the gap between the two define
doors, see "Progress (wave 45)".**

## Progress (wave 45) — lane L5: not built; the gap between the two doors, read

Not built in wave 45: the lane cannot run the proposal's step 1 (the probes
and workloads that must keep their answers), and a define door every
CGLIB / ByteBuddy / Hibernate proxy takes is not a change to make blind. What
the lane read, for the wave that builds it -- `lookup_define.rs`
`lk_define_class_b` (the `Lookup.defineClass` native) against `lang_system.rs`
`native_classloader_define_class0` (what the JDK's `ClassDefiner` reaches,
already the door of every `InnerClassLambdaMetafactory` lambda proxy):

| | `lk_define_class_b` | `defineClass0` |
|---|---|---|
| loader | the lookup class's recorded defining loader (`inherit_lookup_loader`) | `args[0]`, the JDK's `lookupClass.getClassLoader()` (`ClassDefiner.defineClass`), the same loader; `register_defining_loader` is called with it |
| supertypes | resolved through the lookup class (`resolve_lookup_supertypes`) and passed as `superclass_id_override` / `interface_id_overrides`; a real loader throwable propagates | not resolved: the class manager's own supertype resolution |
| `force_loader_faithful_linking` | `true` | default |
| verification | `skip_verification: true` | verified (HotSpot verifies a `Lookup.defineClass` class too) |
| bootstrap / platform lookup class | `builtin_lookup_defines_in_its_package` (privileged define, same package) | `bootstrap_lookup_define` (privileged define; the JDK has already checked the package) |
| name, `ProtectionDomain` | read from the bytes / the lookup class (`--jdk-only`) | the JDK's arguments |
| duplicate define | HotSpot's `LinkageError` (`lookup_duplicate_define_error`, `--jdk-only`) | `classify_duplicate_define` -> `duplicate_define_error` |
| hidden, `NESTMATE`, `STRONG`, class data | the two hidden natives | flags `0x1` / `0x2` / `0x4`, `args[9]` (`attach_class_data`), nest host from `args[1]` |

So step 2 is concrete: `defineClass0` with a non-null lookup class (`args[1]`)
must resolve the supertypes through it (`resolve_lookup_supertypes`'s body
over `class_id_by_name_via_referencing_class(lookup_cid, name)`, with
`absorb_class_absent`) and set `force_loader_faithful_linking`, before the
`Lookup` natives can retire. Whether dropping `skip_verification` is safe for
the generated classes the suite and Spring Boot define needs a run (it is a
HotSpot-parity change in its own right). Then step 3: under `--jdk-only`,
leave `Lookup.defineClass` / `defineHiddenClass` /
`defineHiddenClassWithClassData` to the JDK's bytecode (their registration
must carry an explicit `NativeKind` and a removal record, `AGENTS.md`), behind
a switch that is on by default in its own last commit, and rerun the
proposal's probe list on both builds.

## The problem

`native-builtins/src/lookup_define.rs` registers natives for the WHOLE of
`MethodHandles.Lookup.defineClass(byte[])`, `defineHiddenClass(byte[],
boolean, ClassOption...)` and `defineHiddenClassWithClassData(...)`, in both
modes. JDK 25 does a good deal in Java before it reaches the VM
(`Lookup.defineClass` -> `makeClassDefiner` -> `validateAndFindInternalName`
-> `ClassDefiner.defineClass` -> `JavaLangAccess.defineClass` ->
`ClassLoader.defineClass0`):

* the lookup's modes (`PACKAGE`; full privilege for the hidden forms);
* magic, version (`VM.isSupportedClassFileVersion`), a parse through the
  ClassFile API (`ClassFormatError` with the parse failure as cause),
  `ACC_MODULE`, and the lookup class's package;
* the class-file dumper (`-Djdk.invoke.MethodHandle.dumpClassFiles`), the
  `ClassOption` flags, and the class data.

Each native re-implements a subset. Wave 44 found five rows of
`tools/probes/interp/L5/L5W44LoaderReview.java` on which the natives skipped
the first two groups entirely (a class of another package, or through a
lookup without `PACKAGE`, was defined) and re-implemented them in Rust
(`lookup_define_refusal`). The next JDK change to that Java will not be
seen.

## The direction

Let the JDK's bytecode run down to `ClassLoader.defineClass0(ClassLoader,
Class<?> lookup, String name, byte[] b, int off, int len, ProtectionDomain pd,
boolean initialize, int flags, Object classData)` -- the one native HotSpot
has there -- and keep CratonVM's define (`define_class_full` with the
lookup-inherited loader, supertype identities, nest host, `STRONG`, class
data, the lookup class's `ProtectionDomain`) behind THAT native only
(`classloader.rs` already registers a `defineClass0`). The three `Lookup`
natives, and their `--jdk-only` special cases, then retire.

Order of work:

1. Probe first: `L5W44LoaderReview`'s `lookup-*` / `hidden-*` rows, the
   wave-29 `L5W29LookupDefineDomain`, `L5W29ChildFirstAgentLoader`'s
   `inject` row (JaCoCo's `java.lang.$JaCoCo` through a private lookup on
   `Object`), CGLIB / ByteBuddy / Hibernate proxies, Mockito inline mocks,
   the suite's `RJdkHidden`, and a lambda-heavy workload (the JDK's
   `InnerClassLambdaMetafactory` calls `makeHiddenClassDefiner` directly and
   reaches `defineClass0` already).
2. Make `defineClass0` carry everything the `Lookup` natives do today
   (`resolve_lookup_supertypes`, `inherit_lookup_loader`, the privileged
   bootstrap-package define, `register_non_strong_hidden_class`).
3. Unregister the `Lookup` natives under `--jdk-only` (the registration is
   last-write-wins, so a `NativeKind` and a removal record per
   `AGENTS.md`), measure, then `--compatible` on the owner's call.

## Why not now

The `Lookup` natives exist because the real bytecode once reached a
`defineClass1` with a zero-length byte view (the WP2.3-B note in
`reflect_annotations.rs`); `defineClass0` has to be proven first on every
path above, which needs host runs this lane cannot make.
