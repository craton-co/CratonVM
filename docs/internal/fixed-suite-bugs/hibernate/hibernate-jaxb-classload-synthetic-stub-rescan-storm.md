# HIB-DEV-03 — synthetic-stub upgrade re-scans the whole classpath on every object allocation (✅ FIXED)

> **Status (2026-06-18):** ✅ **FIXED** (worktree `CratonVM-hibxml`, branch
> `fix/hib-dev-03-jaxb-classload`). This was the **dominant** layer of the
> HIB-DEV-03 JAXB-XML-mapping hang. A *separate, deeper* hang sits underneath it
> (the documented JTA / synthetic-socket-loopback cluster) — see
> [hibernate-jta-narayana-xa-completion-and-socket-loopback.md](hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
> **Not** a GC/JIT issue; **not** a collection/`retainAll` issue (the original
> watchdog framing was a red herring — `LinkedHashMap.keySet().retainAll` is
> correct on CratonVM).

---

## Symptom

JAXB/Hibernate XML-mapping test classes time out (`rc=124`) — e.g.
`annotations.xml.ejb3.Ejb3XmlElementCollectionTest`,
`Ejb3XmlManyToOneTest`, `Ejb3XmlOneToOneTest`,
`bootstrap.binding.annotations.access.xml.XmlAccessTest`,
`boot.models.xml.XmlProcessingSmokeTests`. HotSpot (JDK 25) passes them quickly
(`Ejb3XmlElementCollectionTest` = 28 tests, 8.5 s).

The 120 s watchdog dumped the main thread inside
`ClassInfoImpl.findGetterSetterProperties → AbstractCollection.retainAll`, and a
live `cdb` attach showed the active native frames were **class loading**
(`native_map_put → vm_exec::alloc_object → ensure_synthetic_class →
ClassPath::find_class → ZipArchive::by_name → indexmap::get_index_of →
hashbrown::find_inner`). Both pointed at the same root.

## Root cause

`java.util.HashMap`/`LinkedHashMap` node inserts allocate their node objects via
`ctx.alloc_object(ClassId::new(0), NODE_NUM_FIELDS)`
(`../../../../native-collections/src/lib.rs`). `vm_exec::alloc_object` rewrites a
`ClassId(0)`-with-fields allocation to a shared synthetic class
`cratonvm/synthetic/AnonymousObject$N` and calls
`ClassManager::ensure_synthetic_class`.

`ensure_synthetic_class` (and the parallel path in `load_class`) contains a
**synthetic-stub upgrade** step: if the already-registered class is a synthetic
stub, it re-runs `find_class_bytes_delegated(name)` to see whether a *real*
`.class` is now reachable and, if so, upgrades the stub's field layout. That is
correct and useful for real-named stubs (e.g. `java/io/PrintStream`) — but
`cratonvm/synthetic/AnonymousObject$N` can **never** resolve to a real `.class`,
so the upgrade scan **failed and re-ran on every single allocation**.

`find_class_bytes_delegated` walks CDS → bootstrap → extension → **application**
→ IMPL-JARS. With the Hibernate test classpath (~250 application JARs) each scan
is O(num_jars × zip-probes). So **every `HashMap.put` that adds a node did a
full ~250-JAR classpath scan.** JAXB's reflection-driven model building does
hundreds of thousands of puts → a multi-minute "hang". Small classpaths hid it
(the bench/pool apps have a handful of entries), which is why it surfaced only on
the big-classpath Hibernate/JAXB suite.

## Fix

Memoize the absent result. `ClassManager` gains a bounded
`synthetic_upgrade_absent: FxHashSet<String>`:

- `ensure_synthetic_class` / `load_class` skip the upgrade rescan when the name
  is already known-absent (`synthetic_upgrade_known_absent`), and record the name
  on the first failed scan (`note_synthetic_upgrade_absent`).
- The only event that can make a previously-absent name resolvable is a
  classpath extension, so the memo is cleared by `extend_application_classpath` /
  `extend_bootstrap_classpath`. `defineClass` doesn't change classpath
  findability and is reached only after `get_loaded_class_id` misses, so it needs
  no invalidation. This mirrors the JVM's own sticky negative class resolution.

Net effect: each distinct synthetic-stub name does at most one (or two) full
scans, then every later allocation is O(1). All legitimate stub→real upgrades
are preserved (a real class is found on the first attempt when present; the memo
is re-armed whenever the classpath grows).

Files: `../../../../classloading/src/class_manager.rs`.

## Verification (A/B, repro `MapPutStorm.java` = N HashMap.put nodes)

| classpath | baseline (unfixed dev) | fixed | HotSpot |
|---|---|---|---|
| small (1 entry), 20 000 nodes | 4 291 ms | 134 ms | — |
| big (250 JARs), 20 000 nodes | 20 120 ms | 132 ms | 28 ms |
| big (250 JARs), 200 000 nodes | ~200 s (extrapolated) | 1 360 ms | — |

The fixed per-allocation cost is **classpath-independent** (132 ms big-cp ≈ 134 ms
small-cp) — the scan is gone. Baseline cost scaled with classpath size, proving
the storm. Regression: `cargo test -p cratonvm-classloading` 488/488 pass;
`LhmRetainGC` (retainAll-under-GC correctness) green; collection results
unchanged.

End-to-end on `Ejb3XmlElementCollectionTest`: baseline emits `@@BEGIN` then hangs
immediately in the storm; the fixed binary runs the full Hibernate bootstrap + H2
schema setup + a DB commit, then hits the **separate** native-wait wedge (only
the idle Cleaner daemon is dumpable; the work thread is stuck in a native
`WaitForSingleObjectEx`) — the documented JTA / synthetic-socket-loopback Layer-2
hang, which the class-loading storm had been masking.

## Follow-ups

- ✅ **DONE** — the `ClassId(0)`-with-fields path no longer does
  `format!("…AnonymousObject${n}")` + a `class_manager.write()` lock + a
  `class_manager.read()` clamp + an env-var lookup on **every** allocation.
  `SharedVm.anon_class_cache` (a lock-free `[AtomicU32; 256]` indexed by field
  count) caches the resolved synthetic ClassId; the first allocation per field
  count resolves and stores it, every later one does a single relaxed atomic load
  and allocates directly (the stub declares exactly `num_fields` fields, so the
  clamp is a provable no-op). The bigger win is removing the **global
  class-manager write-lock from the allocation hot path** (multi-thread
  scalability), beyond the single-thread micro (~12–20%: 200k-node loop big-cp
  1,553 ms → 1,360 ms). `../../../../vm/src/vm/vm_exec.rs`, `../../../../vm/src/vm/vm_init.rs`.
- The deeper JAXB-test hang is the JTA/XA + synthetic-socket-loopback cluster
  (separate handoff).

## Repro

Run `MapPutStorm <maps> <per>` once with a tiny `-cp` and once with the big
Hibernate classpath (`apps/hibernate-orm/.cratonvm-suite/common.args`); the
unfixed VM's time scales with the number of classpath JARs, the fixed VM's does
not.

```java
import java.util.*;
public class MapPutStorm {
    public static void main(String[] args) {
        int maps = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int per  = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        long t0 = System.nanoTime(); long sink = 0;
        for (int r = 0; r < maps; r++) {
            HashMap<String, Object> m = new HashMap<>();
            for (int i = 0; i < per; i++) m.put("prop" + i, new Object());
            sink += m.size();
        }
        System.out.println("DONE nodes=" + ((long) maps * per) + " sink=" + sink
            + " ms=" + ((System.nanoTime() - t0) / 1_000_000));
    }
}
```
