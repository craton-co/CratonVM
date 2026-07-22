> **Moved to tracked known-issues:** [docs/known-issues/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md](../../../../docs/known-issues/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md)

# HIB-DEV-03 — JAXB XML mapping hangs in `AbstractCollection.retainAll` (`ClassInfoImpl.findGetterSetterProperties`)

**Severity:** High — `rc=124` at the 600s census timeout; the class never completes.
**Status:** 🔴 OPEN — CV-only. **Root re-diagnosed (see update): NOT a localized `retainAll`/collection bug.** Standalone `LinkedHashMap.keySet().retainAll(...)` is fully correct on CratonVM (all sizes, under GC). Live-process `cdb` shows the native activity is in **class loading** (`native_map_put → alloc_object → ensure_synthetic_class → ClassPath::find_class → ZipArchive::by_name → indexmap::get_index_of → hashbrown::find_inner`), triggered by JAXB's reflection-heavy model building. Needs deeper investigation (slow class-load storm vs intermittent zip-index probe) — **handoff**, not a quick localized fix.
**Mode:** Interpreter (JIT-off census).
**HotSpot (JDK 25):** affected classes **PASS** (quickly).

## ⚠️ Update — re-diagnosis (original `retainAll` hypothesis refuted)

The watchdog Java stack (below) shows `AbstractCollection.retainAll`, which led to the initial
"`retainAll` infinite loop" hypothesis. That is **wrong**:

- **Standalone `LinkedHashMap.keySet().retainAll(otherKeySet)` works perfectly** on CratonVM — correct
  results for all removal patterns, map sizes up to 128, and under small-heap GC pressure (repros
  `jsonrepro/LhmRetain{,2,3}.java`, `LhmRetainGC.java`). So the bug is **not** in `retainAll`/LHM.
- **Live `cdb` attach** to the hung process shows the spinning/active native frames are **class loading**:
  ```
  hashbrown::raw::RawTableInner::find_inner          ← active probe
  indexmap::map::IndexMap::get_index_of
  zip::read::ZipArchive::by_name_with_optional_password
  cratonvm_classloading::class_path::ClassPath::find_class / find_in_archive
  cratonvm_classloading::class_manager::ensure_synthetic_class
  cratonvm_vm::vm::vm_exec::alloc_object
  cratonvm_native_collections::native_map_put        ← a Java map.put allocating an object whose class is loaded here
  ```
  i.e. inside JAXB, a `map.put` allocates an object of a not-yet-loaded class → class load → ZIP-archive
  name lookup. Two samples seconds apart showed the stack **move**, so it is progressing (a class-loading /
  reflection storm) rather than wedged in one loop — though a class-load that repeatedly misses/retries, or
  an intermittent hashbrown probe over the zip central-directory index, cannot be excluded without more
  sampling.

**Revised conclusion:** the XML/JAXB classes time out because JAXB's reflection + per-class model building +
CratonVM class-loading (ZIP central-directory `indexmap` lookups) is **very slow on the interpreter**, and/or
an intermittent class-loading hot spot — not a localized `retainAll` defect. The original `retainAll`
framing below is retained only as the symptom that first surfaced it.

---


## Symptom

XML/JAXB-driven mapping tests hang. The stack-dump watchdog (120s) dumps the **main** thread (119 frames); the deepest frames are:

```
depth=114  org/glassfish/jaxb/runtime/v2/model/impl/RuntimeClassInfoImpl.getProperties
depth=116  org/glassfish/jaxb/runtime/v2/model/impl/ClassInfoImpl.getProperties
depth=117  org/glassfish/jaxb/runtime/v2/model/impl/ClassInfoImpl.findGetterSetterProperties
depth=118  java/util/AbstractCollection.retainAll   (pc=33)   ← stuck here
```

`AbstractCollection.retainAll` (pc=33 = inside the `while (it.hasNext()) { … it.remove() }` loop) never returns → infinite loop.

## Root cause (CV collection layer)

`java.util.AbstractCollection.retainAll(Collection c)` is:

```java
boolean modified = false;
Iterator<E> it = iterator();
while (it.hasNext()) {
    if (!c.contains(it.next())) { it.remove(); modified = true; }
}
return modified;
```

It terminates **iff `iterator()` is finite**. JAXB's `findGetterSetterProperties` does a `retainAll` to intersect the getter/setter property name sets. On CratonVM the loop never ends — i.e. the receiver collection's **native-backed iterator never reports `hasNext()==false`** (or `it.remove()` doesn't shrink the collection, so a re-derived iterator keeps yielding). Either way it is a CratonVM `Iterator`/`retainAll`/view-collection defect, not a JAXB bug (HotSpot terminates).

This is the same family as the previously-seen `getSupportedCipherSuites()` + `retainAll` tail issue (Tomcat JSSE) — a `retainAll` / view-iterator mismatch in CratonVM's collection natives.

## Affected classes (CV-only hangs, this dev run)

Confirmed: `annotations.xml.ejb3.Ejb3XmlElementCollectionTest` (watchdog dump above). Same JAXB XML-mapping path, almost certainly the same root (all hang `rc=124`, all JAXB/XML-binding):
`annotations.xml.ejb3.Ejb3XmlManyToOneTest`, `annotations.xml.ejb3.Ejb3XmlOneToOneTest`,
`bootstrap.binding.annotations.access.xml.XmlAccessTest`, `boot.models.xml.XmlProcessingSmokeTests`,
`boot.jaxb.mapping.HbmTransformationJaxbTests` (to confirm individually).

## Next step

Identify the concrete receiver collection at `findGetterSetterProperties`' `retainAll` (a `HashMap.keySet()` / `values()` view, or a custom JAXB collection) and the native `Iterator` whose `hasNext()`/`remove()` is wrong. Likely a localized fix in the collection natives (cf. the JSON `al_state` fix this run). Minimal repro: a `retainAll` over the offending view type should loop standalone.

## Repro

`.cratonvm-suite/repro-xmlhang.txt` = `Ejb3XmlElementCollectionTest`, via `@common.args`; run **without** `CRATONVM_DISABLE_DEFAULT_WATCHDOG` to get the main-thread dump at 120s.
