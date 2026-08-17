# G60-1 — what `--jdk-only` still overrides, counted by the mode itself

**Status:** MEASURED. **Provenance:** one run of `RJdkReflBox` on
`e7e840264` (`C:/craton/target-rel5`) with
`--jdk-only --jdk-only-report`, schema 5. Every number below is read out of
that file. No fix is proposed here and none is made; this is the triage the
nominations in §4 are drawn from.

---

## 0. Why this exists

`--jdk-only`'s premise is that **real class bytes are authoritative**. The mode
already counts its own violations, and nobody in this session had read the
count. One ordinary reflection vector produces:

```text
mode      jdk-only        jdk_feature  25
violations                1422
  synthetic-native-registered   1341     refused at registration — working as designed
  native-shadows-bytecode         81     a native won over real JDK bytecode
counts
  boot_image_classes             513
  application_classes              4
  generated_classes               14
  compatibility_classes            0     <- no class was fabricated
  bridge_invocations            3039
  intrinsic_invocations          757
  synthetic_stub_invocations       0     <- no synthetic stub ever ran
```

The two zeros are the mode's headline and they are **good**: nothing was
fabricated and no synthetic stub executed. The 1,341 refusals are the
enforcement working. **The 81 are what is left.**

## 1. The 81, split by what actually happened

```text
  58  bridge-ran-over-bytecode   the native DISPATCHED in front of real bytes
  21  bridge                     registered over bytecode; not observed running
   2  check-override-name
```

`bridge-ran-over-bytecode` is not a `NativeKind` spelling —
`JDK_ONLY_SHADOW_UNENFORCED_TAG` in `vm/src/vm/vm_exec.rs`. The kind is always
`Bridge`; what the row says is that it ran.

Of the 58, **34 are on classes the VM plainly cannot interpret** —
`jdk/internal/misc/Unsafe`, `jdk/internal/access/SharedSecrets`,
`java/lang/invoke/MethodHandle`/`MethodType`/`MethodHandles$Lookup`,
`java/lang/Class`, `java/lang/foreign/*`. Those are what `Bridge` is for and
this record does not question them.

## 2. The 24 that are pure Java

These are ordinary JDK classes whose bytecode this VM runs elsewhere every day.
Each row is a native that ran **instead of** the real method, MEASURED:

```text
java/lang/Enum.<init>(Ljava/lang/String;I)V
java/lang/ref/ReferenceQueue.poll()Ljava/lang/ref/Reference;
java/lang/ref/WeakReference.<init>(Ljava/lang/Object;Ljava/lang/ref/ReferenceQueue;)V
java/util/ArrayList.get(I)Ljava/lang/Object;
java/util/ArrayList.size()I
java/util/Arrays.asList([Ljava/lang/Object;)Ljava/util/List;
java/util/Arrays.copyOf([Ljava/lang/Object;I)[Ljava/lang/Object;
java/util/Arrays.copyOf([Ljava/lang/Object;ILjava/lang/Class;)[Ljava/lang/Object;
java/util/Arrays.fill([Ljava/lang/Object;Ljava/lang/Object;)V
java/util/Collections.emptyList()Ljava/util/List;
java/util/HashMap$KeyIterator.hasNext()Z
java/util/HashMap$KeyIterator.next()Ljava/lang/Object;
java/util/HashSet.iterator()Ljava/util/Iterator;
java/util/HexFormat.withUpperCase()Ljava/util/HexFormat;
java/util/Properties.getProperty(Ljava/lang/String;)Ljava/lang/String;
java/util/Properties.getProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;
java/util/concurrent/ConcurrentHashMap.<init>()V
java/util/concurrent/ConcurrentHashMap.<init>(I)V
java/util/concurrent/ConcurrentHashMap.get(Ljava/lang/Object;)Ljava/lang/Object;
java/util/concurrent/ConcurrentHashMap.putIfAbsent(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;
java/util/concurrent/CopyOnWriteArrayList.add(Ljava/lang/Object;)Z
java/util/concurrent/CopyOnWriteArrayList.addAll(Ljava/util/Collection;)Z
jdk/internal/constant/ConstantUtils.concat(Ljava/lang/String;Ljava/lang/Object;Ljava/lang/String;)Ljava/lang/String;
jdk/internal/util/Preconditions.checkIndex(IILjava/util/function/BiFunction;)I
```

**Two of these are on a class the retirement table already partially covers.**
`native-api/src/retired_shadow.rs` retires `ArrayList.<init>` (three
overloads), `add` and `clear` — and not `get` or `size`. So the ArrayList work
was done and left half-finished, and the census says so.

**And one of them is the method a whole lane just spent itself on.** G55-1
(`843b80a5a`) fixed `Properties.getProperty`'s lone-surrogate handling inside a
native side table — and this census says a `Bridge` is running over the REAL
`java.util.Properties.getProperty` bytecode, which handles surrogates correctly
by construction because it is `String`-keyed JDK code. That is not a criticism
of G55-1, whose fix is real and whose other five defects are elsewhere. It is
the observation that **nobody checked whether the method needed a native at
all**, and this report answers that question in one line and has been able to
since the flag existed.

## 3. The mechanism already exists

`native-api/src/retired_shadow.rs` is a **per-triple** table — 120 entries
today — that re-tags a shadowing bridge as `SyntheticStub` at registration, so
`--jdk-only` refuses it and the real class runs, while `--real-jdk` is
unchanged (a `SyntheticStub` registers and dispatches normally in Compatible
mode).

Its own doc explains why it is per-triple and not per-class: `Logger.log` has
eight overloads, seven were retired, and the eighth is **not a JDK 25
signature at all** — the real one takes the `Throwable` second — so refusing it
would replace a shadow with an `UnsatisfiedLinkError`. That is the trap any
bulk retirement here walks into.

So the work these 24 rows imply is **entries in an existing table**, not new
machinery. It is also the reason this record proposes no fix: each triple needs
its own answer, and four of them (`ConcurrentHashMap.get`, `ArrayList.get`,
`ArrayList.size`, `HashMap$KeyIterator.next`) are hot paths where retirement is
a performance decision as much as a correctness one.

## 4. Two caveats that bound every number above

* **One vector.** `RJdkReflBox` is a reflection/boxing vector. A different
  workload will shadow a different set; this is a floor for the population, not
  the population.
* **The buffer caps at 256 distinct observations**
  (`JDK_ONLY_NATIVE_SHADOW_CAP`). 81 is comfortably under it here, so nothing
  was dropped in THIS run — but a broad workload can saturate it, and a
  saturated buffer looks exactly like a complete one from the JSON. Anyone
  running this on an application should check the count against the cap before
  reading the list as exhaustive.

## 5. NOMINATIONS

**N1 — finish `ArrayList`.** `get(I)` and `size()` are the two rows left on a
class `retired_shadow.rs` already covers for five other triples. Whoever
retired the five either judged these two differently or missed them, and the
table records no reason. Smallest possible unit of this work, and it settles
whether the hot-path objection in §3 is real.

**N2 — ask whether `Properties.getProperty` needs a native at all.** If the
side table exists to back `System.getProperties()` interop, say so in the
table's doc; if it does not, the real bytecode is already correct on every axis
G55-1 had to fix by hand.

**N3 — run this report against an application, not a vector.** §4's first
caveat is the whole limitation of this record.
`docs/known-issues/jdk-only/APP-READINESS-20260812.md` names the Spring and
Tomcat classpaths that were already used for exactly this kind of run, and its
harness caveats are now cleared (see that file's 2026-08-17 banner), so the
run costs one command.

**N4 — the 21 `bridge` rows that were never observed running.** They are
registered over real bytecode and this vector did not reach them. `invocations`
absence proves nothing (G33-1), so they are neither safe nor unsafe today —
they are unmeasured, which is a different and worse state than either.
