# RESOLVED: JUnit discovery finds 0 tests on CratonVM → now 4/4 (was framed as "JUnit5 jupiter")

## Outcome
FIXED + VERIFIED (2026-06-04). `JUnitProbe` (programmatic `LauncherFactory.discover/execute`)
on CratonVM now reports **`DISCOVER_FOUND=4`, `EXEC_FOUND=4 SUCCEEDED=4 FAILED=0`** —
identical to HotSpot — for `org.apache.commons.math4.transform.TransformUtilsTest`.

## What the bug actually was (the "jupiter" framing was a misdirection)
`TransformUtilsTest` imports `org.junit.Test` (JUnit **4**), so it is discovered by the
**junit-VINTAGE** engine, not jupiter (jupiter correctly finds 0 on both VMs — it's a JUnit4
test). Probe chain that localised it:
- `JUnitProbe` → DISCOVER_FOUND 0 (CratonVM) vs 4 (HotSpot).
- `VintageProbe2` → JUnit4 `Runner.getDescription().getChildren()` = 0 on CratonVM (testCount=4).
- `CollProbe`/`CollProbe2` → **minimal repro**: `new ArrayList<>(concurrentLinkedQueue_with_4)`
  returns size **0** on CratonVM (4 on HotSpot), although `clq.size()/toArray()/iterator()`
  all return 4. JUnit 4.13 `Description.fChildren` is a `ConcurrentLinkedQueue`; its
  `getChildren()` = `new ArrayList<>(fChildren)` → dropped every test method → vintage FOUND=0.

Root cause: in the DEFAULT build (synthetic-jdk OFF) JDK collections run **real bytecode**.
A real ConcurrentLinkedQueue (and LinkedList, LinkedBlockingQueue, …) stores elements in
head/tail Node chains, so the field-layout heuristics in `collect_collection_elements`
(native-collections/src/lib.rs) can't read them → empty. `CollProbe2` confirmed the same bug
hit `new ArrayList<>(linkedList)`, `addAll(clq)`, and `new HashSet<>(clq)` — every
real-bytecode collection source (only `List.of`/ImmutableCollections worked).

## The fix (native-collections/src/lib.rs)
New helper `collect_collection_elements_or_real`: when `collect_collection_elements` returns
empty AND the collection's real `size()` > 0, materialise via its real `toArray()` bytecode.
Recursion-safe (toArray only reaches `native_al_to_array` → `collect_collection_elements`,
never back to `_or_real`; the `size() > 0` guard avoids extra virtual calls on empty
collections). Wired into 4 entry points: `native_al_init_from_collection`,
`native_al_add_all`, `native_hs_init_from_collection`, `native_hs_add_all`.

## Verified
- `CollProbe2`: clq/linkedlist/lbq ctor, `addAll`, `HashSet` ctor — all match HotSpot.
- `JUnitProbe`: discover + execute 4/4, identical to HotSpot.

## Build notes (for re-verification)
- native-builtins is huge; rustc crashes (exit 0xffffffff, no diagnostic) optimizing it at
  opt-level=3/cgu=1 → build with `RUST_MIN_STACK=536870912`.
- A concurrent agent continuously rebuilds native-builtins (holds cargo locks, relinks
  target/release/cratonvm.exe with stale rlibs) → verify with an isolated
  `--target-dir target-verify` build. Verified binary: `target-verify/release/cratonvm.exe`.
- Repro cp (single jupiter-api): /tmp/cm_abs_cp.txt (standalone jar + commons-math
  transform/core target/{classes,test-classes} + numbers/rng/math3 jars). Probes in `bench/`.

## Out of scope (separate bug)
The picocli **console launcher** (`org.junit.platform.console.ConsoleLauncher`) still throws
`ArrayIndexOutOfBoundsException`/NPE in `ConsoleLauncher.run` — the picocli getTerminalWidth /
arraylength issue tracked in `continue_prompt_picocli_arraylength.md`. The programmatic
`LauncherFactory` path (which bypasses picocli) is the authoritative discovery test and passes.
