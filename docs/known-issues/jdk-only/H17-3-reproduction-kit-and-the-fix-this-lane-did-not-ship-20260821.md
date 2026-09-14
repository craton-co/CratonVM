# H17-3 — reproduction kit for the dial, and a specification for the fix this lane deliberately did not ship

**Status: REFERENCE.** Lane H17, 2026-08-21. Companion to `H17-1` and `H17-2`.
Everything here runs against the prebuilt `C:/craton/cratonvm-r8.exe` (clean
build at `025780ff7`) with **no build required**. Oracle HotSpot 25.0.3+9.

`H17-2` §2 found that four of six witnesses this directory has reached for do
not discriminate on a current binary. The point of this record is that the next
lane should not have to rediscover which ones work — and should not have to
re-derive the confound `H0-8` warned about. The probes below are the ones that
survived; they are inlined rather than filed under `regression-suite/probes/`
because that path belongs to no lane in this wave.

---

## 1. Setup

```bash
JDK="$(dirname "$(dirname "$(command -v javap)")")"    # HotSpot 25.0.3+9
"$JDK/bin/javac" *.java

# oracle
"$JDK/bin/java"                --add-opens java.base/java.util=ALL-UNNAMED -cp . <Probe>
# unarmed control  -- ALWAYS run this; it is the arm that must not move
C:/craton/cratonvm-r8.exe --jdk-only --add-opens java.base/java.util=ALL-UNNAMED -cp . <Probe>
# armed
CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap \
C:/craton/cratonvm-r8.exe --jdk-only --add-opens java.base/java.util=ALL-UNNAMED -cp . <Probe>
```

The VM prints `1 per-flag variable(s) set directly; the supported spelling is
now: CRATONVM_LOADER=enforce-native-shadow`. The old spelling still works — every
measurement in `H17-1`/`H17-2` used it — but a lane touching this should
probably move to the supported one.

Add `--jdk-only-report ./rep.txt` for the census. **Read `H17-1` §4 first:** it
is a deduplicated presence set with no counts, and under `enforce` it records
only the bytecode-won half.

## 2. The two rules that make a probe here valid

Both are `H0-8`'s, restated because both were violated by published probes:

1. **One case per process, or take the order as an argument and run it both
   ways.** Anything latched per process is otherwise confounded with case order.
   Every probe below takes an order argument.
2. **State which witness you are reading and what it can see.** The `table`
   array class reports **one bit per map** — whether that map's *first* insert
   ran bytecode. It cannot count yields, and reading it as a count is what
   produced `H16-3`'s, `H0-8`'s and `H17-1`'s shared error.

## 3. The witness that works

`[Ljava.util.HashMap$Node;` (bytecode allocated the table) versus
`[Ljava.lang.Object;` (the native did). Verified sound by `H17-2` §3 — bare
`anewarray` matches HotSpot on every repeat, so a correctly-typed array really
does mean bytecode ran.

```java
static String tableCls(Object map) throws Exception {
    java.lang.reflect.Field f = java.util.HashMap.class.getDeclaredField("table");
    f.setAccessible(true);
    Object t = f.get(map);
    return t == null ? "null" : t.getClass().getName();
}
```

**Do not** use bucket head class, `modCount`, `hashCode()` counts or `equals()`
counts. `H17-2` §2 measured all four as blind.

## 4. The probe that discriminates: door versus position

This is the one that produced `H17-2` §4, the sharpest result of the lane.

```java
import java.lang.reflect.*; import java.util.*;
public class ReflectOrder {
    static String tableCls(Object o) {
        try { Field f = HashMap.class.getDeclaredField("table"); f.setAccessible(true);
              Object t = f.get(o); return t == null ? "null" : t.getClass().getName(); }
        catch (Throwable t) { return "?"; }
    }
    static void direct(String tag) {
        HashMap<String,String> m = new HashMap<>();
        for (int i = 0; i < 4; i++) m.put("k"+i, "v"+i);
        System.out.println(tag+" direct  size="+m.size()+" table="+tableCls(m));
    }
    static void reflect(String tag) throws Exception {
        Method put = HashMap.class.getMethod("put", Object.class, Object.class);
        HashMap<String,String> m = new HashMap<>();
        for (int i = 0; i < 4; i++) put.invoke(m, "k"+i, "v"+i);
        System.out.println(tag+" reflect size="+m.size()+" table="+tableCls(m));
    }
    public static void main(String[] a) throws Exception {
        if (a.length > 0 && a[0].equals("reflectfirst")) { reflect("[1st]"); direct("[2nd]"); }
        else { direct("[1st]"); reflect("[2nd]"); }
    }
}
```

Expected on `r8`, armed for `java/util/HashMap` (MEASURED):

```text
direct first    [1st] direct  [Ljava.util.HashMap$Node;
                [2nd] reflect [Ljava.lang.Object;
reflect first   [1st] reflect [Ljava.lang.Object;
                [2nd] direct  [Ljava.lang.Object;
```

The bottom-right cell is the finding: the reflective door does not honour the
dial **and** leaves nothing for the direct door.

## 5. The probe that separates per-process from per-class

Arm two families at once — `CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/HashMap,java/util/Hashtable`
— populate one map of each, read both `table` fields, and swap the order by
argv. `H17-1` §1 has the full output. `Hashtable`'s field is
`java.util.Hashtable.table`; its oracle table is length **11**, not 16.

This is what showed the yield is not once per process. Note `H17-1` §6: unarmed
`Hashtable` is built by `HashMap`'s minter (length 16, `HashMap$Node` heads),
which is a live divergence in `native-collections` and lane **H23**'s to fix.

## 6. Specification for the fix, since this lane did not ship it

`H17-2` §6 gives the reasons. The specification, so the next lane does not start
from zero:

**The defect, MEASURED (`H17-2` §5).** `jdk_only_enforce_shadow_for` has exactly
one live call site in the repository: `native_override.rs:7433`, in
`resolve_step1_native`. Every dispatch that does not pass through step 1 —
warm invoke-cache entries, the force-native interceptor, reflective
`Method.invoke`, JIT binds — is outside the dial's reach by construction.

**Do the instrumented build FIRST** (`H17-2` N1). Four counters, one per door,
each recording armed-`Bridge` dispatches and whether the dial was consulted.
That build converts every ARGUED claim in `H17-2` into a number and tells you
which door actually carries the volume. Guessing which door to fix without it
is how this lane would have burned its budget.

**Then, per door, the decision is the same three-way one** step 1 already makes:
`strict_bridge`, `enforce`, `shadows_bytecode`. The inputs differ:

* the force-native interceptor already passes `bytecode_available: true` and
  documents why ("concrete bytecode existing is the premise of the call"), so it
  needs the `enforce` term, not a new bytecode probe;
* the cached-dispatch path holds a resolved method, so it can answer
  "has `Code`" without `step1_dispatch_has_code`'s hierarchy walk;
* the reflective path is the one §4 proves is live, and is the place to start.

**Two hazards, both already paid for once in this file:**

* **Per-call-site behaviour drift.** The `java/lang/String` arm was deleted on
  2026-08-04 "because a method's behaviour started depending on how many times
  its call site had run". A dial consulted at a memoized door recreates exactly
  that unless the memo is dial-aware or invalidated when the dial is set. The
  dial is process-static, so **keying the memo on it, or refusing to populate
  the memo while armed, are both available and cheap.**
* **The unarmed arm must not move.** Every edit here is inside an
  `is_jdk_only() && kind == Bridge && enforce` conjunction. If a default
  `--real-jdk` or unarmed `--jdk-only` number changes, the change escaped the
  guard.

**Acceptance.** `--jdk-only` 105/105, `SUITE=all` 103/105 (`RJdkFunctionCombinators`,
`RServiceLoaderDoubleSource` remaining), `SUITE=core` 65/65 — all **unarmed**,
all unchanged. Armed numbers are expected to fall; see `H17-2` §7 for the
predicted direction and its falsifier. `TIMEOUT=600`, and never two
`regression-suite/run.sh` at once — `.guard-tmp` is a fixed shared path.

## 7. What this record does NOT claim

It is a kit and a specification. **Nothing in §6 has been built, run or
verified** — the fix is unwritten by choice, not half-written. The probes in
§§3–5 are the ones whose output is quoted in `H17-1` and `H17-2`; the
`Hashtable` variant in §5 is described rather than inlined, and a lane using it
should re-derive it rather than trust this paraphrase.

## NOMINATIONS

* **N1 — file the surviving probes under a path someone owns.** They are inlined
  here because `regression-suite/probes/` was outside this lane's ownership.
  `ChmConsistencyProbe.java` is still confounded as filed (`H0-8` N4) and should
  be annotated or split in the same pass.
* **N2 — `INDEX.md` was not updated by this lane.** It is shared and this wave
  has several lanes writing concurrently; three new `H17-*` rows are outstanding.
* **N3 — move to the supported flag spelling.** The VM warns that
  `CRATONVM_ENFORCE_NATIVE_SHADOW` is superseded by
  `CRATONVM_LOADER=enforce-native-shadow`. Every armed measurement in this
  directory uses the old spelling; they should be confirmed equivalent once,
  and then the pages should be updated together.
