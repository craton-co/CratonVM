# `getInitiatedClasses` lists a JDK class a constant names that was never resolved

**Status: open — filed 2026-10-08 by interpreter round i1 wave 44, lane L5,
with the fix that closed
`docs/internal/fixed-bugs/interpreter-L3-getinitiatedclasses-ignores-its-loader-FIXED-20261008.md`.
Low: an agent scanning a loader's list to retransform sees more JDK classes
than on HotSpot, none of which it could transform through that loader. Wave
45 (lane L5): no code; the record's buildable shape is in "Progress (wave
45)". Wave 46 (lane L5, `--jdk-only` with a start-up agent): built; a bare
class constant never resolved is no longer listed. What remains -- see
"Progress (wave 46)": a member reference's owner never resolved, a late
(`agentmain`) agent, the application loader's `Class.forName`.**

## Progress (wave 46) — lane L5: the record, for the class door

* **The record.** `ClassRealm::jdk_names_initiated` (per non-bootstrap
  loader, a set of `ClassId`s): the JDK classes (bootstrap-defined, or
  platform-defined for a loader other than the platform loader; an array
  records its element) that the loader's classes' `CONSTANT_Class`
  resolutions answered. Written by
  `runtime::resolve::initiating_records::record_jdk_name_initiated`: from
  the class-resolution door (`constants.rs` `resolve_class_loader_aware`,
  now a wrapper over `resolve_class_loader_aware_unrecorded` that calls
  `note_class_resolution` on success -- CROSS-LANE, lane L4's file) and from
  a user loader's `Class.forName` (`vm_exec.rs` `note_initiating_load`). A
  class already recorded costs one read guard. Swept at class unload
  (`memory::gc`) with the other initiating records.
* **Armed only while it can be read.** `ClassRealm::jdk_names_initiated_armed`
  is set by `agent_loader::invoke_premains` when there is a start-up agent,
  under `--jdk-only`, before any agent or application code runs; every
  other VM pays one relaxed load per class resolution and writes nothing.
* **The list** (`vm_exec.rs` `list_initiated_class_ids`), when armed: the
  record, plus the constant-pool scan restricted to the class entries a
  field or method reference names (member owners: the invoke and field
  doors resolve them by the receiver or by name, not through the class
  door, so the record does not see them). Not armed (no start-up agent, or
  `--compatible`): the wave-44 scan, unchanged.
* Probe `tools/probes/interp/L5/L5W46InitiatedUnresolvedConstant.java`
  (agent jar): HotSpot `unresolved=false resolved=true new=true array=true`;
  the base prints `unresolved=true`. `L5W44InitiatedJdkClasses` must keep its
  HotSpot lines (its `ldc` rows go through the record now).

**What remains:**

* A member reference's owner that was never resolved (a never-called method
  invoking `java.util.concurrent.atomic.LongAdder.sum()`) is still listed
  from the scan. Recording owners needs a write in the invoke and field
  doors (lane L4's files), or the per-entry table of
  `i46-L5-proposal-one-resolved-class-entry-table-every-door-writes-20261010.md`.
* An agent attached late (`agentmain`, self-attach through
  `instrument.rs`) finds the record unarmed and gets the wave-44 scan.
* `Class.forName(jdkName, init, loader)` with the APPLICATION loader
  (HotSpot files the class in its dictionary): only a user loader's
  `forName` reaches `note_initiating_load`.
* Measure: the armed write on the class door (one read guard per
  resolution miss of a JDK name) under a `-javaagent` workload
  (Mockito inline mocks, JaCoCo).

## Progress (wave 45) — lane L5: no code; the record's shape, and why not a scan

No change in wave 45. Re-read against `69568bea6`:

* **Option 2 (ask each constant's own resolution record) has no record to
  ask.** The class's `ConstantPool` (`reader/src/constant_pool.rs`) holds
  only the parsed entries; a resolved `CONSTANT_Class` is remembered by the
  door that resolved it (the `ldc` constant cache, the `new` / `checkcast` /
  `instanceof` site caches, the member caches), as the page says; the lane
  found no one table keyed by `(class, cp index)` with a "resolved to"
  answer a list can read. Building such a read API means
  touching every door; option 1 is smaller.
* **Option 1, made cheap.** The record needs writing only while an
  `Instrumentation` can ask, which is a per-VM fact: write the loader's
  initiated JDK name in `constants.rs` `resolve_class_loader_aware`'s success
  arm (lane L4's file) when the VM has an `Instrumentation` (one relaxed load
  of a per-VM flag set when `sun.instrument.InstrumentationImpl` is created,
  as `instrument::transformers_armed` is read today), the resolving class's
  loader is not the bootstrap loader, and the answer is a bootstrap- or
  platform-defined class; into a per-VM `RwLock<FxHashMap<ClassLoaderId,
  FxHashSet<ClassId>>>` next to `ClassRealm::initiating_records`, checked
  under the read lock first so a name already recorded costs no write.
  `list_initiated_class_ids` then reads it instead of scanning the constant
  pools. A `-javaagent` agent's `Instrumentation` exists before the
  application's first class is loaded, so its list is complete; an agent
  attached late (`agentmain`) would miss earlier resolutions, for which the
  wave-44 scan is the fallback (use the scan when the flag was set after the
  first application-loader define).
* **Also needed for HotSpot's list:** `Class.forName(name, init, loader)` of a
  JDK name with an application or user loader files it in that loader's
  dictionary too (`SystemDictionary::resolve_or_fail` with that loader);
  `lang_class::native_class_for_name` would write the same record.
* Probe for the row this page is about (not written: it needs an agent jar):
  a class of the application loader with a never-called method naming
  `java.util.concurrent.ConcurrentHashMap` by `ldc`; HotSpot's application
  list lacks it, CratonVM's has it.

## What happens

Since wave 44, `NativeContextImpl::list_initiated_class_ids`
(`vm/src/vm/vm_exec.rs`) adds to a non-bootstrap loader's
`Instrumentation.getInitiatedClasses` list every LOADED bootstrap- or
platform-defined class that a `CONSTANT_Class` entry of a class the loader
defined names. It cannot tell a resolved entry from one never executed: the
VM keeps no per-entry "resolved" record for class constants in one place
(the `ldc` record, the `new` / cast site caches and the member-resolution
caches each keep their own), and it records no initiating loader for a JDK
name.

HotSpot lists such a class only when the loader initiated it: a resolution
through one of its classes' constant pools, or a load its verifier made
through the loader for an assignability check.

## Evidence (trace, not run)

A class `A` defined by the application loader with a method that is never
called and names `java.util.concurrent.ConcurrentHashMap` in an `ldc`:
HotSpot's application-loader list does not hold `ConcurrentHashMap` (the
entry is never resolved, and an `ldc` of a class needs no verifier load);
CratonVM's does, since the JDK's own start-up loads that class, so
`loaded_class_under_exact_key("java/util/concurrent/ConcurrentHashMap",
Bootstrap)` finds it.

## What would fix it

Either of:

1. A per-loader record of the JDK names its classes resolved, written where
   a class constant's resolution first succeeds (`constants.rs`
   `resolve_class_loader_aware`), read by the list. That is the write on
   every JDK name's first resolution the wave-40 page declined; it is
   `i41-L1-proposal-one-initiating-loader-record-for-every-loader`'s table.
2. Ask each constant's own resolution record at list time: the `ldc` record
   (`cached_cp_constant`), the `new` / `checkcast` / `instanceof` site caches
   and the member-resolution caches, by `(class, cp index)` -- a scan of
   several tables that each would need a read API for "resolved to which
   class".

The verifier's loads are a HotSpot implementation detail no probe should
assert.

## Host result after wave 46 (orchestrator, 2026-09-30)

On the landed head (`a03e7a9d5`, `--jdk-only`, JIT and `--nojit`)
`L5W46InitiatedUnresolvedConstant` matches HotSpot, and
`L5W44InitiatedJdkClasses` prints `app string=false` where HotSpot prints
`app string=true`. Before wave 46 the row matched only by accident: every
bare class constant was listed whether it had run or not, and the probe's
`String.class` constant runs after `getInitiatedClasses` returns.

`CRATONVM_DBG_RETRANSFORM=1` on the host shows the application loader's list:
6 recorded JDK classes plus the scanned member owners (`Object`, `System`,
`PrintStream`, `ConcurrentSkipListSet`, `java.sql.Date`, `Class`, `Method`,
`Instrumentation`, `StringConcatFactory`, `ClassLoader`, `InputStream`,
`Throwable`), and no `java/lang/String` line. The follow-up commit's
`ldc`-record arm did not fire, because that `ldc` had not run yet.

What HotSpot records instead: the application loader initiates `String`
while it links the class's descriptors. Candidates are the string-concat
call site's `MethodType` (its parameter and return types resolve through the
caller's loader), `main(String[])`, and the verifier's assignability loads.
CratonVM resolves descriptor types without recording the initiating loader.
The next step is to record the initiating loader wherever a descriptor's
reference types are resolved through a class's loader (method type
resolution for `invokedynamic` and `ldc MethodType`, and the verifier), then
re-run both probes. `L5W44InitiatedJdkClasses` row `app string` is the
positive control; `L5W46InitiatedUnresolvedConstant` must keep
`unresolved=false`.
