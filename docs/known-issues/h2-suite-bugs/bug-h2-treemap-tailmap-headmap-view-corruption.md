# `java.util.TreeMap.tailMap()`/`headMap()` submap views are broken — first element is `null` or the view is spuriously empty

## Status
**OPEN** — new finding, 2026-07-21. Not H2-specific; a general `java.util.TreeMap`
correctness bug, reproducible with pure `java.util.TreeMap` and no H2 code
at all. Reproduces identically with JIT on and `--nojit` — an interpreter
(or object-model), not JIT-codegen, bug.

**Correction (2026-07-22):** the "Leading hypothesis" below
(`find_by_method_descriptor` reachability) is **refuted** — confirmed via a
full-crate grep while root-causing
`bug-h2-nosuchmethoderror-cross-class-dispatch-FIXED.md` that
`NativeMethodRegistry::find_by_method_descriptor` has no callers anywhere in
the dispatch path (dead code, only referenced by its own definition and
tests). That doc's own two clusters turned out to be a different, unrelated
mechanism (native construction writing into a hardcoded object field-slot
index that collides with a real-JDK class's actual field at that index) that
does not apply here (`TreeMap` has no native registration of any kind, so
there is no construction-time field write for it to collide with). This
doc's actual root cause is still open and needs a fresh hypothesis — the
"same underlying dispatch defect" connection to that other doc was informed
speculation, not a confirmed shared cause, and should not be assumed.

## Severity
**HIGH** — `TreeMap`/`NavigableMap` submap views are a common, ordinary
JDK collections idiom. Any code that iterates `tailMap`/`headMap`/(likely)
`subMap` results gets either a `null` element or an incorrectly-empty view.

## Affected test classes
All four fail identically via H2's own in-memory filesystem
(`org.h2.store.fs.mem.FilePathMem`, backed by a `static final TreeMap<String,
FileMemData> MEMORY_FILES`), whose `newDirectoryStream()` iterates
`MEMORY_FILES.tailMap(name).keySet()`:
- `org.h2.test.db.TestPowerOff` (`testLobCrash`, at `Database.close()` →
  `deleteOldTempFiles()`).
- `org.h2.test.db.TestDiskFull`
- `org.h2.test.synth.TestPowerOffFs`
- `org.h2.test.unit.TestReopen`

All PASS on the HotSpot JDK25 baseline. All reproduce single-threaded, with
no concurrent modification of `MEMORY_FILES` in flight — ruling out the
obvious "unsynchronized concurrent mutation" explanation (every mutation of
`MEMORY_FILES` in H2's source is already inside `synchronized (MEMORY_FILES)`).

## Symptom
```
java.lang.NullPointerException: Cannot invoke "String.startsWith(String)" because "n" is null
	at org/h2/store/fs/mem/FilePathMem.newDirectoryStream(FilePathMem.java:91)
	at org/h2/store/fs/FileUtils.newDirectoryStream(FileUtils.java:195)
	at org/h2/engine/Database.deleteOldTempFiles(Database.java:1582)
```
from H2's own:
```java
public List<FilePath> newDirectoryStream() {
    ArrayList<FilePath> list = new ArrayList<>();
    synchronized (MEMORY_FILES) {
        for (String n : MEMORY_FILES.tailMap(name).keySet()) {   // <- "n" comes back null
            if (n.startsWith(name)) { ... }
        }
        return list;
    }
}
```

## Root cause (narrowed, not fully pinned to an exact interpreter code line)
Isolated to plain `java.util.TreeMap`, no H2 involved:
```java
TreeMap<String,Integer> m = new TreeMap<>();
for (int i = 0; i < n; i++) m.put("key"+i, i);   // any n >= 5
for (String k : m.keySet()) { ... }              // fine, every element correct
for (String k : m.tailMap("key").keySet()) { ... }  // FIRST element is null
```
Confirmed under CratonVM (`cratonvm-h2-fail-triage-20260721
--java-home /home/victor/jdk25`, with and without `--nojit`) at every size
tested (5, 10, 20, 50, 100, 200, 300, 500) — the plain `keySet()` iterator
is always correct; the `tailMap(K).keySet()` iterator's first element is
always `null`. `headMap(K).keySet().iterator().next()` throws
`NoSuchElementException` immediately instead (spuriously-empty view, a
related but distinct symptom). Both pass cleanly on the HotSpot JDK25
baseline. `java.util.concurrent.ConcurrentSkipListMap.tailMap(K)` on the
same host, for comparison, works correctly.

**Leading hypothesis, not fully confirmed:** `java.util.TreeMap` itself has
no native override registered anywhere in `native-builtins` — its
`tailMap`/`headMap`/`subMap` run as real, unmodified JDK bytecode
(`AscendingSubMap`/`NavigableSubMap` and their private iterators). However,
`native-builtins/src/lib.rs` *does* register natives for
`java.util.concurrent.ConcurrentSkipListMap`'s `tailMap`/`headMap`/`subMap`
(backed by `cslm_subrange`, expecting CSLM's own 3-slot field layout:
0=backing array, 1=size, 2=capacity). `native-api/src/registry.rs` also
documents a class-agnostic recovery fallback,
`NativeMethodRegistry::find_by_method_descriptor`, explicitly built for "a
synthetic native-allocated object... routed through `java/lang/Object`
because the heap reports `class_id_of` as 0" — matched purely by
`(method_name, descriptor)`, deliberately ignoring the receiving class, and
documented as "first registration wins on key collision". This is the same
general shape of bug already tracked as OPEN elsewhere in
`docs/known-issues/README.md` (the "wrong-receiver-type/virtual-dispatch"
family referenced from `jit-osr-linux-regression-triad.md` and
`http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md`'s
"`(class, method, descriptor)`-substituting `NoSuchMethodError`"). A real
`TreeMap`'s field layout does not match CSLM's, so if a `TreeMap` instance's
submap-view construction is ever routed through this recovery path (or a
similarly class-blind lookup elsewhere in the dispatcher not identified in
this session), reading CSLM's expected field 0/2 off a real `TreeMap`
object would plausibly yield exactly this: a degenerate view that returns
one garbage/null key before ending, or reports itself empty.

This session did not get far enough into the interpreter's invokevirtual
dispatch path to confirm the exact mechanism connecting `TreeMap.tailMap`
call sites to the CSLM-shaped handler — flagged as the next step, not
chased further given the fail-triage time budget. See also
`bug-h2-nosuchmethoderror-cross-class-dispatch.md`, whose symptom (an
explicit `NoSuchMethodError` citing an unrelated class) may share this same
underlying dispatch defect, though the two present differently (silent
corrupt data here vs. an explicit but wrong-class exception there) and are
kept as separate docs pending confirmation they're the same root cause.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestPowerOff
```
or, standalone (no H2):
```java
import java.util.TreeMap;
public class T {
    public static void main(String[] a) {
        TreeMap<String,Integer> m = new TreeMap<>();
        for (int i = 0; i < 20; i++) m.put("key"+i, i);
        System.out.println(m.tailMap("key1", true).keySet().iterator().next());
    }
}
```
