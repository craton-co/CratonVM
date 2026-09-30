# Proposal: one "resolved to" table per class-constant entry, written by every resolution door, read by the questions that need JVMS §5.4.3's record

**Status: proposal — filed 2026-10-10 by interpreter round i1 wave 46, lane
L5. Not implemented. The most promising direction lane L5 saw in round i1
for its area.**

## The problem

JVMS §5.4.3 resolves a symbolic reference once and records the answer: a
`CONSTANT_Class` entry, once resolved, IS a class, and the resolving class's
loader is from then on an initiating loader of it. HotSpot keeps that record
in the resolved-references and resolved-klasses arrays of the constant pool
(and files the class in the loader's dictionary). CratonVM has no such table:
each door keeps its own memo of what it resolved -- the `ldc` record, the
`new` / cast / `anewarray` site caches, the member-resolution cache
(`ClassRealm::resolution_cache`, per `(class, cp index)` of a member
reference), the invoke doors' receiver-driven owner selection, the
`invokestatic` owner's own drive (`dispatch_static.rs`). Round i1 needed the
answer "did this loader's class resolve this entry, and to what" four times
and had to approximate it each time:

* `Instrumentation.getInitiatedClasses` (waves 42-46): the list scanned the
  loader's constant pools and over-listed every JDK class a never-executed
  constant names; wave 46 records the class door's answers
  (`ClassRealm::jdk_names_initiated`, only while a start-up agent can read
  it) and still scans the member-owner entries, because the invoke and field
  doors resolve owners without that door
  (`i44-L5-getinitiatedclasses-lists-a-jdk-class-an-unresolved-constant-names`);
* loader constraints: a transparent loader's view of a JDK-global name is
  "what it resolved", which no table says
  (`interpreter-L5-loader-constraints-are-not-imposed-at-member-resolution-FIXED-20261010.md`,
  moved to `i41-L1-proposal-one-initiating-loader-record-for-every-loader`);
* JDWP `ClassLoaderReference.VisibleClasses` and JVMTI
  `GetClassLoaderClasses` (the `i41-L1` proposal's readers);
* a resolution failure's record (JVMS §5.4.3's "subsequent attempts fail
  with the same error") is kept per door too
  (`record_member_owner_failure_as_of`, `recorded_resolution_failure`).

## The direction

1. A per-VM table `(referencing ClassId, cp index) -> Resolved(ClassId) |
   Failed(error)` for `CONSTANT_Class` entries only (the member entries keep
   `resolution_cache`, keyed the same way). Written on each door's MISS,
   never on a hit: `resolve_class_loader_aware`'s success and failure (the
   one door most sites share since wave 46's wrapper), the `invokestatic`
   owner, the receiver-driven owner of `invokevirtual` / `invokeinterface`
   and the field doors' owner at their first resolution. Dropped with the
   class at unload, and per class at redefinition (a new pool).
2. Readers ask it instead of their approximations: `getInitiatedClasses`
   lists the JDK classes the entries of a loader's classes resolved to
   (exactly HotSpot's dictionary rows from resolution); the loader-constraint
   view of a transparent loader is the class its classes' entries resolved to
   for that name; the failure records become one row kind of the same table.
3. Measure the write: it is one insert per entry per class, on the miss
   path only (the site caches keep answering hits). Wave 46's
   `jdk_names_initiated` is the same write for JDK names only and behind an
   agent flag; the table would replace it.

## Probes

`tools/probes/interp/L5/L5W44InitiatedJdkClasses.java`,
`L5W46InitiatedUnresolvedConstant` (plus a member-owner row: a
never-called method invoking `java.util.concurrent.atomic.LongAdder.sum()`,
which HotSpot does not list and wave 46's scan still lists), the
`L5W42GlobalNameConstraint` `transparent` row, and the owner-failure probes
(`L5W24OwnerFailureRecord`, `L5W25OwnerFailureRecordInvoke`) must keep
HotSpot's lines.
