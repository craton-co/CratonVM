# Proposal: one initiating-loader record for every loader, in both modes

**Status: open — filed 2026-10-05 by interpreter round i1 wave 41, lane L1.
Not built. It takes over the last item of
`docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md`
(`ClassLoaderReference.VisibleClasses`), and would serve step 2 of
`docs/internal/fixed-bugs/interpreter-L3-getinitiatedclasses-ignores-its-loader-FIXED-20261008.md`.
The table is lane L5's; its readers are lanes L1 and L3's.**

## The problem

Three questions ask the same thing — the classes a loader is an initiating
loader of (JVMS §5.3: the loader defined them, or the VM asked it for the
name and it answered another loader's class) — and CratonVM answers none of
them from such a record:

* **JDWP `ClassLoaderReference.VisibleClasses`** (`debug::inspect::visible_classes`)
  lists every class whose defining loader is the loader named or one of its
  ancestors: every class that loader could name by delegation. HotSpot lists
  the classes that loader initiated: 41 for the application loader in
  `tools/probes/interp/L1/L1W24JdiSurface.java` (wave 24's measurement),
  thousands here. JDI resolves a field's, a local's and a method's declared
  type through that list (`ClassLoaderReferenceImpl.findType`, JDK 25's
  `jdk.jdi`), so on HotSpot `Field.type()` of a loaded JDK type the program
  never resolved through its loader throws `ClassNotLoadedException`, and on
  CratonVM it answers the type. The probe asks membership only for that
  reason.
* **`Instrumentation.getInitiatedClasses(loader)`**
  (`i40-L3-getinitiatedclasses-ignores-its-loader`): ignores its argument.
* **JVMTI `GetClassLoaderClasses`**: not in the C table
  (`vm/src/jvmti/native_env.rs`) at all.

The one record there is (`runtime::resolve::initiating_records`, wave 38,
lane L5) holds, by design, only user-defined loaders, only names outside the
JDK-global namespaces, and only under `--jdk-only`: it exists for
`findLoadedClass` and the §5.3.5 duplicate-definition check. HotSpot's list
for the application loader is mostly the rows it leaves out
(`java.lang.Object`, `java.lang.String`, ...).

## Design

1. **A second, read-only-by-queries record** beside
   `ClassRealm::initiating_records`, or that table widened with a flag per
   row saying whether the define and `findLoadedClass` readers may use it:
   `(loader, name) → class` for every loader (built-in ones included) and
   every name, in both modes, written where HotSpot writes its dictionary
   entry — the class-resolution door's successful drive through a loader
   (`constants.rs` `drive_defining_loader_load_named` and the built-in
   loaders' fast paths that answer without a drive), `Class.forName` with a
   loader (`NativeContext::note_initiating_load`), and the supertypes a
   define resolves through the defining loader. `--compatible` must record
   too for the debugger's answer, but no existing reader's answer may change
   there (the §5.3.5 check and `findLoadedClass` keep reading only the
   `--jdk-only` user-loader rows).
2. **Readers:** `VisibleClasses` answers the loader's defined classes plus
   its rows (and the primitive array classes, which HotSpot lists for every
   loader); `getInitiatedClasses` the same through
   `list_initiated_class_ids`; a JVMTI `GetClassLoaderClasses` could be added
   to the C table on top.
3. **Cost:** one insert per first resolution of a `(loader, name)` pair (a
   resolution is cached per constant-pool entry afterwards), and memory per
   pair. It must not sit on any per-bytecode path; measure the class-loading
   heavy rows (Spring Boot start, the jdk-only corpus) before and after.

## Expected benefit

`VisibleClasses` and `getInitiatedClasses` answer HotSpot's lists; JDI's
`findType` throws `ClassNotLoadedException` exactly where HotSpot's does; an
agent that scans a loader's classes to retransform them sees that loader's.

## How to verify

`L1W24JdiSurface` with a count line added (HotSpot: 41 for the application
loader) and a `ClassNotLoadedException` row (a field of a JDK type the
program never resolves); `tools/probes/interp/L3/L3W40InitiatedClassesByLoader.java`.

## Risk

Low for correctness (read-only consumers), but the record's writers sit on
the resolution door, which every class-loading measurement crosses.

## Progress (wave 46) — lane L5: carries the i29-L5 transparent-loader remainder; a first JDK-name record

* **Moved here** from
  `docs/internal/fixed-bugs/interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`
  (closed in wave 46): a transparent user loader's global-route answer
  (a loader `drive_defining_loader_load` does not ask, whose chain answers a
  JDK-global name as the global route does) is checked against the
  constraint table (`constants.rs` `resolve_class_loader_aware`,
  `check_initiating_load`, wave 42) but not recorded as the loader's view.
  Until a constraint names the pair, `loader_constraints::constraint_view`
  does not see that the loader resolved the name, so a later cross-loader
  resolution records a constraint instead of comparing, and refuses the
  loader's NEXT resolution of the name rather than the one already made.
  This table is the record `constraint_view` needs ("not loaded" for a name
  the loader never resolved, the class for one it did).
* **Built in wave 46, for `getInitiatedClasses` only:**
  `ClassRealm::jdk_names_initiated` (per non-bootstrap loader, the JDK
  classes its `CONSTANT_Class` resolutions and a user loader's
  `Class.forName` answered), written by the class-resolution door's
  wrapper (`initiating_records::note_class_resolution`) and only while a
  start-up agent armed it (`agent_loader::invoke_premains`, `--jdk-only`).
  It is not this proposal's table: it is empty without an agent, holds JDK
  classes only, and misses member owners (the invoke and field doors
  resolve those without the class door). See
  `i46-L5-proposal-one-resolved-class-entry-table-every-door-writes-20261010.md`
  for a per-entry table that would serve both.
