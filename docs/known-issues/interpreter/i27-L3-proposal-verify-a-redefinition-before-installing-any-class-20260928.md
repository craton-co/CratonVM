# Proposal: verify every class of a redefinition before installing any

**Status: stage 1 landed (wave 28, lane L3); stage 2 open — filed 2026-09-28
by interpreter round i1 wave 27, lane L3.**

## Progress (wave 28) — lane L3

Stage 1 landed, with two departures from the design below:

* **No clone of the live `Class`** (it is not `Clone`, and cannot be: its
  `ConstantPool` is not). `ClassManager::check_redefinition` now ends in
  `verify_redefinition_ahead`, which builds a scratch `Class` field by field
  (`redefinition_scratch_class`: the live class with the new methods, the NEW
  constant pool and bootstrap methods) and verifies it under the redefinition's
  own policy, now one function both paths call (`verify_redefined_class`: the
  structural half for a user loader's class under `loader_aware_resolution`,
  `verify_class` otherwise). The hierarchy is the one Step 5b uses (the class
  resident, `in_flight: None`): the scratch class has the live class's shape,
  so no hierarchy question can tell them apart. The merged pool is not built:
  the old constants it appends are never indexed by the new bodies and the
  verifier never walks the pool as a table.
* **The type maps the verifier publishes go to a throwaway store**
  (`ScratchTypeMaps`, a `ClassHierarchy` wrapper whose `type_map_store` is its
  own). Publishing is first-writer-wins keyed by class id, so a check of a class
  that had no maps yet would otherwise have published the NEW bodies' maps for
  the LIVE old bodies: wrong oop maps whenever the call was then refused.
* **The token is a note, not a type.** The check records `(fingerprint of the
  bytes, redefinition generation)` per class (`pre_verified_redefinitions`, a
  mutex because the check runs under the class-manager read lock; bounded, and
  dropped on unload). `redefine_class_typed` takes the note after Step 1 and
  skips Step 5b when both match, so a redefinition verifies once; bytes a native
  `ClassFileLoadHook` substituted, or a class redefined in between, miss the
  note and are verified as before. The rollback stays as the backstop.

Verified by `classloading/tests/redefine_verify_policy.rs` (three new tests:
the check refuses an under-declared `max_stack` for an application class and
installs nothing; it keeps the deferred Pass 3 for a user loader's class; the
note is taken by the redefinition of the same bytes and not trusted for other
bytes) and by `tools/probes/interp/L3/L3W28VerifyBeforeInstall.java` (agent
jar): HotSpot 25 prints `first=old second=old` for a two-class
`redefineClasses` and `retransformClasses` whose second class fails
verification; CratonVM printed `first=new second=old` (read from the code).

What remains: stage 2, firing the native JVMTI `ClassFileLoadHook` in the
check (lane L1's side; needs a C agent to test).

## Problem

Since wave 27 `redefineClasses` / `retransformClasses` check every class of a
call before redefining the first (`vm/src/runtime/instrument.rs`,
`install_redefinitions`, over `ClassManager::check_redefinition`), so a
structural refusal of any class leaves all of them unchanged, as HotSpot's
`VM_RedefineClasses::load_new_class_versions` does. Two refusals still come
only from the redefinition itself, after earlier classes of the call were
installed:

* **verification.** `ClassManager::redefine_class_typed` verifies the new
  bodies after installing them in the live `Class` (Step 5b), and rolls back
  on failure; `check_redefinition` cannot verify without installing.
* **a native JVMTI `ClassFileLoadHook`** (Step 1, `fire_class_file_load_hook`)
  may substitute other bytes, which the check never saw.

Evidence that the first matters: every CratonVM-only redefinition refusal on
record was a verification refusal of a Mockito-woven class
(`docs/internal/fixed-suite-bugs/springboot/twelve-unclustered-residuals-20260905-FIXED.md`,
`java/lang/Object`; `mockito-bytebuddy-classfile-metadata-cluster-FIXED-20260805.md`;
`infinispan-configurationbuilder-retransform-verify-FIXED.md`). Mockito
retransforms a whole hierarchy in one call; a refusal of its third class now
throws with the first two woven, where HotSpot changes none.

## Design

1. `ClassManager::check_redefinition` builds a scratch `Class`: a clone of the
   live one with the new methods (in the live order, as Step 5 permutes them),
   the merged constant pool and bootstrap methods, and runs the same verifier
   policy Step 5b does (`verify_class_structure` +
   `verify_class_structural_bytecode` for a user loader under
   `loader_aware_resolution`, `verify_class` otherwise) against the unchanged
   store. The live class is not touched; `redefine_class_typed` then skips its
   own verification when handed a checked definition (a token from the check,
   stamped with the class's redefinition generation so a redefinition in
   between invalidates it).
2. Fire the native `ClassFileLoadHook` in the check (as HotSpot fires it while
   loading the new versions) and hand its output to the redefinition, which
   then does not fire it again.

## Expected win and how to measure

Correctness: `L3/L3W27RedefinitionRefusals` gains a row where the SECOND
class of a two-class `redefineClasses` fails verification; HotSpot prints
`first=old`, CratonVM today `first=new`. No performance change on any path
that does not redefine; a redefinition verifies once, as now.

## Cost and risk

Cloning a `Class` is cold-path work proportional to the class; the verifier
must not read the live `Class` by id while checking the scratch one (the
hierarchy adapter resolves supertypes, never the class itself, but that needs
an audit: `ClassStoreHierarchy` with `in_flight` is the precedent for a class
not in the store). Medium risk; the rollback path stays as a backstop.

## Staged plan

1. The scratch-class verification in `check_redefinition`, behind the token;
   the new probe row.
2. The `ClassFileLoadHook` move (needs a C agent to test; lane L1 owns the
   JVMTI side).
