# Bug TC0622 — `Hashtable.clone()` casts a synthetic native entry to `Hashtable$Entry` ("cratonvm/synthetic/AnonymousObject$4 cannot be cast to java/util/Hashtable$Entry")

> **Root cause (one line):** It is **not** a JNDI/LDAP gap. JNDI gets far enough to
> construct `InitialDirContext`; the failure is in **`java.util.Hashtable.clone()`**.
> CratonVM models `Hashtable`/`Properties` **natively** (entries live in a native
> bucket store / slot-0 `table[]` populated with synthetic objects, *not* genuine
> `java/util/Hashtable$Entry`), but `Hashtable.clone()` is **not** registered as a
> native — it runs the **real-JDK bytecode**, which does
> `t.table[i] = (Hashtable$Entry) table[i].clone()`. The synthetic entry CratonVM
> stored (`cratonvm/synthetic/AnonymousObject$4`) is not a `Hashtable$Entry`, so the
> `checkcast` throws `ClassCastException`. The JNDI `InitialContext(Hashtable)` ctor
> clones its environment Hashtable (`InitialContext.java:206`), so the very first
> real `new InitialDirContext(env)` blows up before any LDAP socket is attempted.

**Severity:** Medium (breaks any code path that clones a natively-modelled
`Hashtable`/`Properties`; here it gates JNDIRealm startup, but the cast is generic
to all Hashtable-clone callers — e.g. all `javax.naming` `InitialContext`/
`InitialDirContext` construction, which always clones the environment).
**Status on CratonVM:** FAIL (1 of 4 tests). **HotSpot:** PASS (4/4).
**Run date:** 2026-06-23
**Binary:** dev `df11ac00` (worktree `C:/craton/CratonVM-tctest`, exe
`cratonvm-tcfull-0622.exe`).

**Affected class / test:**
`org.apache.catalina.realm.TestJNDIRealm#testErrorRealm` (line ~126).

The other three tests in the class **PASS** —
`testAuthenticateWithoutUserPassword`, `testAuthenticateWithUserPassword`,
`testAuthenticateWithUserPasswordAndCredentialHandler` — because they subclass
`JNDIRealm` and **override `open()`** to return a mocked `DirContext`, so they
never reach `createDirContext` / `new InitialDirContext`. Only `testErrorRealm`
exercises the real `createDirContext` path (it deliberately points at a dead
port `ldap://127.0.0.1:12345` to test error handling) and therefore is the only
one that hits the `Hashtable.clone()` cast. So **all observed failures share a
single root cause**, and that root cause is the Hashtable-clone modeling mismatch,
**not** LDAP/JNDI connectivity.

## Symptom

`realm.start()` throws `LifecycleException: Failed to start component
[JNDIRealm[...]]`, with the nested cause being a `ClassCastException` inside
`Hashtable.clone()` — *not* a `NamingException`, *not* an unsatisfied-link /
"no code" for an LDAP/JNDI native, and *not* a context-factory-not-found:

```
org.apache.catalina.LifecycleException: Failed to start component [JNDIRealm[...]]
    at org.apache.catalina.util.LifecycleBase.start(LifecycleBase.java:185)
    at org.apache.catalina.realm.TestJNDIRealm.testErrorRealm(TestJNDIRealm.java:126)
Caused by: java.lang.ClassCastException: cratonvm/synthetic/AnonymousObject$4
                                          cannot be cast to java/util/Hashtable$Entry
    at java.util.Hashtable.clone(Hashtable.java:565)
    at javax.naming.InitialContext.<init>(InitialContext.java:206)
    at javax.naming.directory.InitialDirContext.<init>(InitialDirContext.java:130)
    at org.apache.catalina.realm.JNDIRealm.createDirContext(JNDIRealm.java:2690)
    at org.apache.catalina.realm.JNDIRealm.open(JNDIRealm.java:2660)
    at org.apache.catalina.realm.JNDIRealm.get(JNDIRealm.java:2615)
    at org.apache.catalina.realm.JNDIRealm.startInternal(JNDIRealm.java:2869)
[cratonvm] System.exit(1) called — process terminating
```

`Tests run: 4, Failures: 1` (`....E`).

The log also carries the tell-tale GC-guard fingerprint of a synthetic/`Object`
receiver being read with a non-matching layout, immediately before the throw:

```
WARN cratonvm::gc::guard: gen_heap::get_field: out-of-bounds field read dropped
  obj=0x1eafa660 index=0 num_slots=0 class_id=ClassId(0)
  class_name=java/lang/Object real_field_count=Some(0)
```

i.e. a `java/lang/Object`-typed / zero-slot synthetic value is being treated as a
structured map entry — consistent with a native bucket entry that the real-JDK
clone bytecode then casts to `Hashtable$Entry`.

## Root cause (pinned)

1. **Trigger path.** `JNDIRealm.startInternal` → `get` → `open` →
   `createDirContext(env)` (`JNDIRealm.java:2690`) calls
   `new InitialDirContext(env)`. `env` is a `Hashtable<String,String>` that
   JNDIRealm fills with the JNDI connection properties (`INITIAL_CONTEXT_FACTORY`,
   `PROVIDER_URL`, auth, etc.).

2. **The JDK clones the environment.** `InitialContext(Hashtable environment)`
   does `environment = (Hashtable) environment.clone();` (`InitialContext.java:206`,
   confirmed in JDK 25 `src.zip`). So the *very first thing* JNDI does with the
   env Hashtable is `clone()` it — long before any LDAP socket connect. This is
   why the failure is independent of the dead port: it would fire against a live
   LDAP server too.

3. **CratonVM models Hashtable natively, but not `clone`.** CratonVM registers
   native `<init>`/`put`/`get`/`keys`/… for `java/util/Hashtable` and `Properties`
   (`native-collections/src/lib.rs` ~26630+); `put` writes entries through the
   shared native bucket path (slot-0 `table[]` plus a native side-store), and the
   entry objects it allocates are **synthetic** (`alloc_synthetic(...)`, surfacing
   as `cratonvm/synthetic/AnonymousObject$4` / `HashMap$Node`-shaped), **not** real
   `java/util/Hashtable$Entry`. There is **no** native registration for
   `Hashtable.clone()` (grep: zero `"clone"` hits in `native-collections`).

4. **Real-JDK `clone()` casts the synthetic entry → CCE.** With no native
   override, `Hashtable.clone()` runs the real bytecode (JDK 25
   `Hashtable.java:560-572`):

   ```java
   t.table = new Entry<?,?>[table.length];
   for (int i = table.length ; i-- > 0 ; ) {
       t.table[i] = (table[i] != null)
           ? (Entry<?,?>) table[i].clone() : null;   // <-- checkcast Hashtable$Entry
   }
   ```

   It reads `table[i]` (the synthetic `AnonymousObject$4` CratonVM stored) and
   `checkcast`s it to `Hashtable$Entry`. The synthetic object is not a
   `Hashtable$Entry`, so the cast throws `ClassCastException`, which JNDI/Catalina
   wrap as the `LifecycleException`.

**In short:** a native-bucket vs. real-JDK-bytecode layout mismatch. CratonVM's
Hashtable is natively backed, but a real-JDK method (`clone`) reaches into
`table[]` expecting genuine `Hashtable$Entry` instances. This is the same class of
defect as the documented `Properties`/`HashMap$Node` slot-layout and
`AnonymousObject$N` out-of-bounds-field issues — a method that walks the native
buckets as if they were the real JDK node type.

## Reproduction

Run the one class (use run-suite.ps1's jvm args — context won't start otherwise):

```powershell
cd C:\craton\CratonVM\apps\tomcat
$CP = (Get-Content .tooling\cp.txt -Raw).Trim()
$env:CRATONVM_REAL_NET_SOCKETS=1
$env:CRATONVM_REAL_AQS=1
$env:CRATONVM_DISABLE_DEFAULT_WATCHDOG=1
C:\craton\CratonVM-tctest\target\release\cratonvm-tcfull-0622.exe `
  -Dtomcat.test.basedir=C:\craton\CratonVM\apps\tomcat\output\build `
  -Dtomcat.test.tomcatbuild=... -Dtomcat.test.temp=... `
  --add-opens java.base/java.util=ALL-UNNAMED `
  -cp $CP org.junit.runner.JUnitCore org.apache.catalina.realm.TestJNDIRealm
```

Minimal standalone repro (no Tomcat/JNDI needed) — clone a Hashtable that was
populated through the native put path:

```java
java.util.Hashtable<String,String> h = new java.util.Hashtable<>();
h.put("a", "b");
Object c = h.clone();   // HotSpot: OK;  CratonVM: ClassCastException
                        //   cratonvm/synthetic/AnonymousObject$N -> java/util/Hashtable$Entry
```

(Equivalently `new javax.naming.InitialContext(env)` with any non-empty `env`.)

## Recommendation

**FIX (bounded) — likely the right call, not a JNDI/LDAP handoff.** The defect is
*not* in JNDI/LDAP support: JNDI reaches `InitialDirContext` construction fine, and
the LDAP layer is never the issue. The fix is local to the Hashtable native model.
Two viable approaches:

1. **Register a native `Hashtable.clone()`** (and, since `Properties extends
   Hashtable`, cover that too) that builds a fresh natively-backed Hashtable and
   copies the entries via the same native bucket accessors that `put`/`keys` use —
   bypassing the real-JDK bytecode that casts to `Hashtable$Entry`. This mirrors
   how other native-collection methods already shadow real-JDK bodies
   (`keys()`/`elements()`/`putAll` notes in `native-collections/src/lib.rs`).
   Purely additive; the real-JDK `clone()` path is currently 100% broken for any
   natively-populated Hashtable, so there is no behavior to preserve.

2. **(Larger)** Make the native put path allocate genuine `java/util/Hashtable$Entry`
   instances into slot-0 `table[]` (analogous to the existing `HashMap$Node`
   real-layout work at `lib.rs:3284+`) so *all* real-JDK Hashtable bytecode
   (`clone`, serialization, `entrySet` reflection) reads them correctly. Higher
   value but broader blast radius; option 1 unblocks `TestJNDIRealm` with minimal
   risk.

**Not JIT, not GC.** The cast fires deterministically on the structural type of the
bucket entry (every clone of a populated Hashtable), independent of JIT/GC timing;
the `gc::guard` WARN lines are a symptom of the synthetic/`Object` receiver layout,
not a timing-dependent corruption.
