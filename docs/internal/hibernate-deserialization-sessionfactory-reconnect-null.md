# HIB-DEV-05 — deserialized `EntityManager`/`SessionFactory` has null SessionFactory → NPE (5 classes)

**Severity:** Medium — fails Hibernate's serialization round-trip tests.
**Status:** ✅ **RESOLVED 2026-06-20** — root cause was reflective `Method.invoke` retargeting a **private** method to a subclass override. Fixed in `native-builtins/src/lang_class.rs` (`native_method_invoke`): private instance methods now dispatch via `invoke_special` (no virtual retarget). Real Hibernate 6.5 + H2 `EntityManager`/`EntityManagerFactory` serialize→deserialize→use round-trip now PASSES on CratonVM (interpreter and JIT); the serialized EM stream is byte-identical in structure to HotSpot (2216 bytes; only the random per-run UUIDs differ).
**Mode:** Interpreter (JIT-off) — also verified with JIT on.
**HotSpot (JDK 25):** all 5 **PASS**.

## ✅ Resolution (2026-06-20) — reflective `Method.invoke` retargeted a private method to a subclass override

The earlier refinements (below) were on a **dead path**. The Hibernate suite runs against a real `--java-home`, where CratonVM executes the **real** `java.io.ObjectOutputStream` / `ObjectStreamClass` bytecode (the synthetic `serialization.rs` natives are dormant — confirmed: zero hits during the run, and the produced classdesc field names are written by real `ObjectStreamClass.writeNonProxy` via `writeUTF`). So the "synthetic OOS object-graph encoding" / offset-divergence theory was a red herring (it reflected an older config where the synthetic OOS was active).

**Actual root cause.** `java.io.ObjectStreamClass.writeSerialData` walks the class-data layout slot-by-slot (super-first) and, for each slot with a `writeObject` hook, calls `slotDesc.invokeWriteObject(obj, oos)` which does `writeObjectMethod.invoke(obj, {oos})` (reflection). For a `SessionImpl` graph the slots are `[AbstractSharedSessionContract, SessionImpl]`, both with a **private** `writeObject`. `AbstractSharedSessionContract` is an **abstract** class.

CratonVM's `Method.invoke` native (`native_method_invoke`) correctly skipped virtual dispatch for private methods (`use_virtual_dispatch = !is_static && !is_private && !is_init`) but then called the resolved method through `ctx.invoke(class_name, …)` → `invoke_on_class_shared`. That helper **retargets** a call whose declaring class is **abstract/interface** onto the receiver's concrete class (a correct fix for abstract/interface methods with no `Code`, but **wrong** for a private concrete method that merely lives in an abstract class). So `invoke(AbstractSharedSessionContract.writeObject, sessionImpl)` ran **`SessionImpl.writeObject`** instead.

Consequence: the `AbstractSharedSessionContract` slot's hook (which writes `factory.serialize(oos)` = the SessionFactory UUID via `oos.writeUTF`) was never invoked; instead `SessionImpl.writeObject` ran **twice** (its persistence-context/action-queue serialization appears twice in the trace; the UUID `writeUTF(len=36)` never appears). On deserialization, `SessionFactoryImpl.deserialize(ois)` read an empty UUID → `SessionFactoryRegistry.findSessionFactory("")` missed → `factory = null` → NPE on first use.

**Why earlier minimal repros (MultiLevelSer, PrivInvoke*) passed:** their superclass was a **concrete** class, so the abstract-class retarget never fired. The trigger is specifically a **private method declared in an abstract (or interface) class, reflectively invoked on a concrete-subclass receiver** (returned `SUB` pre-fix, `BASE` after). The committed regression test is `vm/tests/hib_dev_05_private_abstract_dispatch.rs`.

**Fix.** In `native_method_invoke`, route private instance methods through `ctx.invoke_special` (→ `invoke_on_class_shared_no_retarget`) instead of `ctx.invoke`. `invoke_special` resolves to the declaring class with no retarget — exactly the invokespecial semantics a private method requires. One-branch change; all reflection repros (PrivInvoke/2/3, SlotRepro, AbstractPriv) and serialization repros (MultiLevelSer, InheritedFields, Utf2/3) pass, and the real Hibernate EM/EMF round-trip passes.

---

## Historical investigation (superseded — kept for context)

## Symptom

`NullPointerException: Cannot invoke getMappingMetamodel on null` / `Cannot invoke getClassLoaderService on null`, after a serialize→deserialize round-trip:

```java
EntityManager em = scope.getEntityManagerFactory().createEntityManager();
out.writeObject(em);
em = (EntityManager) in.readObject();   // deserialized em's SessionFactory is null
// subsequent use -> SF.getMappingMetamodel()/getClassLoaderService() NPE
```

**Affected (CV-only):** `jpa.ejb3configuration.EntityManagerSerializationTest`,
`jpa.ejb3configuration.EntityManagerFactorySerializationTest`,
`jpa.serialization.EntityManagerDeserializationTest`,
`connections.SuppliedConnectionTest`, `connections.BasicConnectionProviderTest`.

## Root cause (narrowed)

The generic Java deserialization machinery works on CratonVM — a standalone `Serializable` with a private
`readObject(ObjectInputStream)` **and** `readResolve()` is correctly invoked (repro `jsonrepro/SerRepro.java`,
matches HotSpot). So the failure is specific to **Hibernate's SessionFactory reconnection**: a serialized
`SessionImpl`/`SessionFactoryImpl` stores the factory UUID + name and, on deserialization, reconnects to the
live factory via `SessionFactoryRegistry.INSTANCE.findSessionFactory(uuid, name)`. On CratonVM that lookup
yields **null**, so the deserialized object's `factory` field stays null → NPE on first use.

Candidate causes (needs the SessionFactoryRegistry path traced): the registry's UUID/name keying or its
backing map lookup not matching on CratonVM, or the SF not being registered under the expected key. (Note:
diagnosis was impeded by the reversed-stack-trace bug — see
[FAIL-throwable-stacktrace-order-reversed.md](../../apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/FAIL-throwable-stacktrace-order-reversed.md), now fixed, which
should make the precise NPE site visible on a re-run.)

## Refinement 2026-06-18 (#2) — PINNED to a serialized-stream STRUCTURE divergence in `SessionImpl` (deep core-serialization bug)

With the throwable-order fix in, the deser NPE now has a **readable stack**:
```
NullPointerException: Cannot invoke "SessionFactoryImplementor.getMappingMetamodel()" because the receiver is null
  … ObjectInputStream.readObject
  at org/hibernate/internal/SessionImpl.readObject(SessionImpl.java:2843)
  at org/hibernate/engine/internal/StatefulPersistenceContext.deserialize(StatefulPersistenceContext.java:1942)
```
`StatefulPersistenceContext.deserialize` (called from `SessionImpl.readObject`) calls `session.getFactory()
.getMappingMetamodel()` and **`session.getFactory()` is null** — the `factory` field (set by
`AbstractSharedSessionContract.readObject` → `SessionFactoryImpl.deserialize(ois)` →
`locateSessionFactoryOnDeserialization(uuid,name)`) never got reconnected.

**Root pinned via a minimal probe** (`jsonrepro/EmUuidProbe.java`, real `Configuration.buildSessionFactory()`
+ `createEntityManager()` + serialize): the SF uuid **is** present in CratonVM's serialized `EntityManager`
stream — but **at a completely different offset**:

| | serialized `EntityManager` stream | factory-uuid offset |
|---|---|---|
| HotSpot | 3197 bytes | **1244** (early, super's region) |
| CratonVM | 3087 bytes | **2863** (near the end) |

So CratonVM's `ObjectOutputStream` writes the SessionImpl object graph in a **different structure** (the
factory lands ~1600 bytes later, total 110 bytes shorter). On read, `SessionFactoryImpl.deserialize` consumes
the stream at the position the *bytecode* expects (early) but CratonVM put the factory data elsewhere → it
reads the wrong bytes → `locate` misses → `factory = null` → NPE.

**Ruled out as the cause** (all match HotSpot on CratonVM): class-hierarchy `writeObject`/`readObject`
ordering at **2 levels** (`jsonrepro/HierSer.java`) **and 3 levels** (`jsonrepro/Hier3.java`) — super-first,
correct offsets; `ObjectInputStream.writeUTF`/`readUTF` round-trip (`jsonrepro/UtfProbe.java`); the
`writeReplace`/`readResolve`/`findSessionFactory` reconnect of a SF serialized **directly**
(`jsonrepro/SfRegProbe.java`, `deser SF == original`). So it is **not** any of those — it is a
**field/handle/back-reference ORDER divergence** in CratonVM's `ObjectOutputStream` for a complex,
deep object graph (the difference only manifests on the real SessionImpl, not on 2–3 level toy hierarchies).

**Conclusion:** confirmed, precisely located — but a **deep core-serialization bug** in CratonVM's
`ObjectOutputStream`/`ObjectStreamClass` object-graph encoding, not a localized native fix. Next step is
serialization-stream forensics: dump and diff the TC_OBJECT/TC_CLASSDESC/TC_REFERENCE structure of the CV vs
HotSpot `EntityManager` stream to find the field/handle whose position diverges (the ~1600-byte shift before
the factory). Repros in `.cratonvm-suite/jsonrepro/`.

---

## Refinement 2026-06-18 — generic serialization mechanisms all verified working (gap is Hibernate-specific)

Attempted to localize; **ruled out** every generic cause (all match HotSpot on CratonVM):
- `readObject(ObjectInputStream)` + `readResolve()` invoked correctly (`jsonrepro/SerRepro.java`).
- **`writeReplace()` → proxy → `readResolve()` → registry-lookup reconnect** round-trips correctly and returns
  the **same** live object (`jsonrepro/ReplaceRepro.java`, `back==original`). This is exactly the shape of
  Hibernate's `SessionFactoryImpl` serialization (writeReplace → `SerializableSessionFactory(uuid,name)` →
  readResolve → `SessionFactoryRegistry.findSessionFactory`).
- `ConcurrentHashMap<String,_>` lookup by an **equal-but-distinct** String key (the deserialized UUID) returns
  the value (`jsonrepro/ChmKey.java`) — so `findSessionFactory(uuid)` = `sessionFactoryMap.get(uuid)` is sound.
- **No CV native/stub** intercepts `SessionFactoryRegistry` / `addSessionFactory` / `findSessionFactory` — it
  runs real Hibernate bytecode.

`findSessionFactory(uuid,name)` decompiles to `sessionFactoryMap.get(uuid)` (then a `nameUuidXref`/name
fallback). Since the map + key-equality + reconnect mechanics are all correct, the residual must be
**Hibernate-specific**: either the SF was never `addSessionFactory(uuid,…)`-registered during CratonVM
bootstrap, or the UUID in the serialized form ≠ the registered UUID (or a name/`nameUuidXref` step). Pinpointing
needs a full EMF-bootstrap reproduction that inspects `SessionFactoryRegistry.INSTANCE` state (the JUnit
`EntityManagerFactoryScope` makes a standalone probe non-trivial). **Not a quick localized native fix** —
remains a handoff.

## Next step

Re-run with the stack-order fix to capture the exact NPE frame; then trace
`SessionFactoryRegistry.findSessionFactory(uuid, name)` on CratonVM (registry population at SF build vs the
UUID/name used at deserialization). Likely a registry-keying or UUID bug — possibly localized, but not yet
pinpointed.
