# Hand-off: keycloak Jackson "Conflicting getter" bug (RESOLVED 2026-05-30)

## RESOLUTION (root cause + fix)
**Root cause:** CratonVM models `java.util.*` maps natively (`synthetic-jdk`,
on by default). `Map.keySet()` / `entrySet()` / `values()` returned a
**detached snapshot** (a fresh `HashSet`/`ArrayList`), so mutating through the
view — `view.remove`, `view.iterator().remove()`, `removeIf` — never reached
the backing map. Jackson's `POJOPropertiesCollector._renameProperties` does
`props.entrySet().iterator().remove()` then re-`get`s the same key; the dead
snapshot left the entry in `props`, so the renamed builder was merged with
itself (`POJOPropertyBuilder.addAll` → `merge` concatenates the `_getters`
chain), giving the property TWO getters wrapping the same `Method` →
`getGetter()` throws "Conflicting getter definitions … getY() vs getY()".
This is why the HARD EVIDENCE below saw 2 `AnnotatedMethod`s over one Method.

**Fix** (`native-collections/src/lib.rs`): the view snapshots now carry a
reference to the *source* map and the shared removal natives propagate to it
(live-view semantics), routing through the map's own `remove` via
`invoke_virtual` so HashMap/LinkedHashMap/TreeMap/CHM sources are each handled
correctly. keySet/entrySet snapshots use a dedicated synthetic backing
(`cratonvm/util/MapViewBacking`, source + view-kind in high slots);
`native_hs_remove` deletes the key (or `entry.getKey()`) from the source.
`values()` and the ArrayList-backed TreeMap entrySet stash the source in the
element array's last capacity slot; `native_al_itr_remove` /
`native_al_remove_obj` / `native_al_remove_if` propagate via
`propagate_list_removal` (entrySet by key, values by value). TreeMap keySet
keeps its sorted array-backed `TreeSet` with the source in the element array's
trailing slot; `native_ts_itr_remove`/`native_ts_remove`/`native_ts_clear`
propagate. A missing `TreeSet$Itr` arm in the `Iterator.remove` dispatcher
(`native_itr_remove_noop`) was also added so `TreeSet.iterator().remove()`
(and TreeMap keySet iteration) deletes instead of throwing UOE — a pre-existing
bug surfaced by this work.

Verified: `pkgtest.PkgRepro` → `{"y":42}` on both interpreter and JIT paths;
HashMap/LinkedHashMap/TreeMap keySet/entrySet/values iterator-remove +
removeIf + view-remove + clear all write through (probes pass), sorted order
preserved; `cargo test -p cratonvm-vm` green.

----
(original investigation notes follow)

## Symptom
Keycloak core tests fail under CratonVM (`--nojit`): Jackson throws
`InvalidDefinitionException: Conflicting getter definitions for property "X":
C#getX() vs C#getX()` during `ObjectMapper.writeValueAsString`.
Baseline: `org.keycloak.JsonParserTest` 10 run / 7 fail; `SkeletonKeyTokenTest`
5 run / 5 fail — all this error. HotSpot: all pass.

## Minimal reproducer (no keycloak needed)
```java
package pkgtest;                       // also repros in default package
public class PkgRepro {
  public static class A { @JsonProperty("y") private int y;
    public int getY(){return y;} public void setY(int v){y=v;} }
  // new ObjectMapper().writeValueAsString(new A())  → THROWS on CratonVM, OK on HotSpot
}
```
Run: `cratonvm.exe --java-home <jdk25> --stack-dump-on-timeout 0 -cp "<dir>;<jackson 2.17.2 databind+core+annotations>" pkgtest.PkgRepro`
(`CRATONVM_DISABLE_JIT=1`).

## HARD EVIDENCE (cleanest probe, both VMs side-by-side — _jx style)
Reaching into Jackson `POJOPropertyBuilder._getters` (private linked list) for
property "y":
```
CRATON:  getter[0] AnnotatedMethod.id=7918  reflectMethod.id=7126
         getter[1] AnnotatedMethod.id=7919  reflectMethod.id=7126   <-- TWO wrappers
         total getters in chain = 2                                     SAME Method (7126)
HOTSPOT: getter[0] ... total getters in chain = 1
```
=> Two distinct Jackson `AnnotatedMethod` objects wrap the **same** underlying
`java.lang.reflect.Method`. Jackson's per-property getter list ends up with 2
entries; `POJOPropertyBuilder.getGetter()` then can't disambiguate → throws.

## RULED OUT (each measured == HotSpot)
- **Duplicate methods in reflection tables.** `getDeclaredMethods` getY = 1,
  `getMethods` getY = 1, classfile `javap` getKeys = 1. Not a dup-method bug.
  (Three separate dedup attempts in `collect_public_methods` /
  `declared_methods_with_synthetic` were therefore NO-OPS and were reverted.)
- **String.hashCode / MemberKey.hashCode nondeterminism.** Stable across 5
  calls, byte-identical to HotSpot (`"getY".hashCode()`=3189040,
  MemberKey=3189069). `MemberKey.equals` = true for two keys of the same method.
- **Class-name corruption.** A *packaged* class (`pkgtest.PkgRepro$A`) reports
  the correct name and STILL fails — so the name bug below is unrelated.

## THE CONTRADICTION to resolve next
`MemberKey` dedup provably works in isolation (equal + same hashCode), yet
`AnnotatedMethodCollector` ends with two `AnnotatedMethod`s for one Method.
That should be impossible if the collector keys every method through the same
`Map<MemberKey, MethodBuilder>`. So the second getY must enter via a collection
pass that bypasses or mis-keys the dedup map. Likely suspects (jackson-databind
2.17.2 `com.fasterxml.jackson.databind.introspect`):
- `AnnotatedMethodCollector._addMemberMethods` main-class vs mix-in/superclass
  passes — does CratonVM make `A` appear in the type hierarchy twice (e.g.
  `getGenericSuperclass`/type-resolution returning a second view of `A`)?
- `AnnotatedClassResolver` / `TypeFactory` building the supertype list — a
  duplicated `JavaType` for `A` would drive a second `_addMemberMethods(A)`.
- NOTE: some later probe runs gave CONTRADICTORY counts (keycloak
  `memberMethods getKeys = 1` while Jx$A `_getters = 2`). The probe environment
  was corrupting output (partial file writes, cross-run contamination, ANSI
  escape bleed, processes exiting mid-print) — RE-MEASURE in a clean shell
  before trusting any single number. The `_jx` getter-chain dump is the most
  reliable datum; reproduce it first.

## EXACT NEXT EXPERIMENT
1. Re-run the `_jx`-style getter-chain dump on `pkgtest.PkgRepro$A` (packaged)
   to confirm 2-vs-1 reliably in a clean shell (write Java output to a file via
   FileWriter; do NOT rely on cratonvm stdout — it bleeds ANSI/TUI escapes).
2. Instrument the COLLECTION side: dump `AnnotatedClass.memberMethods()` getY
   count AND, via reflection, how many times `_addMemberMethods` sees `A` — i.e.
   print the supertype list Jackson walks (`TypeFactory`/`AnnotatedClassResolver`).
   If `A` (or a `JavaType` for it) appears twice → fix CratonVM's
   `getGenericSuperclass`/`getGenericInterfaces`/type reflection that feeds it.
3. If memberMethods=1 but `_getters`=2, the dup is created in
   `POJOPropertiesCollector._addGetterMethod` — trace which two AnnotatedMethods
   it adds and why `_getters` doesn't collapse them.

## Suspect CratonVM natives (start here once collection side is pinned)
`native-builtins/src/lang_class.rs`:
- `collect_public_methods` (getMethods) — already dedups? (no; reverted)
- `native_class_get_generic_superclass` / `get_generic_interfaces` / generic
  type reflection — if these return a duplicate/!= supertype view, Jackson
  walks `A` twice.
- `mirror_class_name` / class-name natives (see SEPARATE bug below).

## SEPARATE bug found in passing (default-package class name)
`A.class.getName()` for a class in the **default (unnamed) package** returns
`java.Coll$A` on CratonVM vs `Coll$A` on HotSpot — a phantom `java.` package is
prepended. Packaged classes are unaffected. Likely in the class-name
construction for no-package classes (`mirror_class_name` / binary-name builder
in `native-builtins/src/lang_class.rs` or the classloader). Low priority (most
real code is packaged) but a real spec violation worth a separate fix.

## Status of THIS session's committed work (all on dev, none pushed)
- `06dc83a` gc+invoke: H2 System.gc() stale-root crash + JUnit-4 ctor SIGSEGV
- `d1726ea` verifier: reflection regression (store.get-before-insert false reject)
- `b8278ba` classloading: redefine force-decode method attrs (Code visible to verifier)
classloading test suite fully green. The keycloak annotation bug is UNFIXED.
