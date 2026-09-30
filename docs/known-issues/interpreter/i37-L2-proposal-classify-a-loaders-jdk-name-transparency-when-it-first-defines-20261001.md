# Proposal: classify a user loader's JDK-name transparency when it first defines a class

**Status: proposal — filed 2026-10-01 by interpreter round i1 wave 37, lane L2.**

## Problem, with evidence

`ClassRealm::loader_global_name_transparency` (wave 28, lane L5) records,
per user-defined loader, whether it answers every runtime-image name exactly
as the JDK does (`classloader_real::loader_answers_jdk_names_as_the_jdk`: no
`loadClass` override on its delegation chain). It is filled lazily, and only
by the interpreter: `constants.rs` `drive_loader_for_global_name`, on the
first resolution miss of a `javax/` / `jdk/` / `sun/` / `com/sun/` name from
one of the loader's classes, because the classifier reads the loader object
through a `NativeContext`, which needs a `JvmThread`.

The JIT has no thread at compile time and cannot fill it. Wave 37 (lane L2)
traced the recommended JIT fix of
`interpreter-L5-a-user-loaders-class-under-a-jdk-name-is-resolved-to-the-jdks-class-FIXED-20261005.md` (fixed-bugs)
(defer a JDK-global name a non-transparent loader has not answered) and
found that an UNCLASSIFIED loader must be treated as non-transparent there:
the fix would then also defer every such name for transparent loaders the
interpreter happened not to classify yet (`why=transparency-unknown` in the
`CRATONVM_DBG_ISOLATED_CNF` census it landed). That is one of the two reasons
the fix waits for a measurement.

## Design

Classify the loader where a thread and the loader object are both at hand
and the loader is about to matter: the first `defineClass` of a class by a
user-defined loader (the define natives in `native-builtins`, lane L5's
files), inserting into the same per-VM map. The verdict is a pure function
of the loader's class and its `parent` chain, which do not change after
construction (a loader's `parent` field is `final`), so classifying early
loses nothing. `drive_loader_for_global_name`'s lazy arm stays as the
fallback for a loader defined by another route.

## Expected win and how to measure it

Removes the `transparency-unknown` deferrals from the i26 JIT fix, so its
cost reduces to the genuinely non-transparent loaders. Measure with the
wave-37 census: `grep -c 'why=transparency-unknown'` on the Spring Boot run
before and after; the target is zero. Cost: one walk of at most 16 loader
classes' declared methods per user loader, once.

## Risks

A define that happens during the loader's own construction (a subclass that
defines classes from its constructor) could see a `parent` not yet set; the
classifier then answers from a partial chain. Classify only when the
`parent` field is already written, or keep the lazy path for that loader.
