# F11-1 — Reflective boxing is canonical everywhere except `Array.get`, and the caller that forbade the obvious fix was never a caller

**2026-08-13, lane F11.** Applies F1-1's NOMINATION 1. Fixes
`lang_class::box_value`'s callers in `native-builtins/src/lang_class.rs`, the
only source file this lane edited (plus this record). **This lane did not build
or run CratonVM**; every "before" is a fact read out of the tree and every
CratonVM "after" is explicitly PREDICTED. Every HotSpot value below was
MEASURED on this host against `openjdk 25.0.3 2026-04-21 LTS (25.0.3+9-LTS)`
(Microsoft build) from `scratchpad/f11/BoxCallers.java`, `BoxCallers2.java` and
`BoxCallers3.java` before any of it was written down.

---

## 1. Verdict

| | |
|---|---|
| the nominated defect | `box_value`'s doc comment said it produced `Integer.valueOf(42)`; every arm called `alloc_wrapper` and allocated fresh |
| reflective paths whose identity was measured on HotSpot | **8 caller families, 70 printed rows** across three probes (`Field.get`, `Array.get`, `Method.invoke`, MethodHandle return adaptation, MethodHandle collector/varargs element boxing, `VarHandle.get` in five shapes, `SerializedLambda` captured args, `InvocationHandler` `args[]`) |
| of those, paths HotSpot answers **CANONICAL** | **7 of 8** |
| of those, paths HotSpot answers **FRESH** | **1** — `Array.get`, on *both* VMs |
| `box_value` call sites in `lang_class.rs` (this lane's file) | **3** — all three measured canonical, all three switched |
| `box_value` call sites elsewhere | **21** in `lang_invoke.rs` (plus one in its test at `13046`), 1 in `cglib_enhancer.rs`, 1 in `craton_gpu.rs` — measured, tabulated, **not edited** (§7 N1) |
| a **fourth** boxing implementation found | `vm/src/vm/vm_exec.rs::proxy_box_value{,_for_desc}` — measured canonical on HotSpot, allocates fresh here, and its `Boolean` arm does not return `Boolean.TRUE` (§7 N2) |
| new cached storage added | **none** — the split routes into the six caches that already exist (§5) |
| `RJdkIntrinsics3 --only=boxid` rows affected | **0 of 69** (§6.1) — and that is a mechanical result, not a skim |
| regression-suite rows anywhere that assert reflective boxing identity | **0** (§6.2) — this whole surface is unasserted (§7 N3) |
| NOMINATIONS raised | **4** |

**The premise the lane was handed is half wrong, and the half that is wrong
matters.** The brief said routing all of `box_value` through the caches "fixes
one caller and breaks the other", `Array.get` being the other. `Array.get` on
this VM **does not call `box_value` at all** — `lib.rs::native_array_get`
inlines its own `alloc_wrapper` per component type (`lib.rs:40938`–`41022`), and
`Array.getInt`/`getChar`/… return bare primitives. So the unified fix would
have been *observationally* safe today, and the split is still right: it is
right because the fresh contract now has a named helper, a doc comment that
says which HotSpot function it mirrors, and a **negative-control test** that
fails if someone caches it — none of which the accidental safety provided.
A guard that holds only because a caller happens to be wired elsewhere is the
`[premise=guard]` shape.

## 2. The oracle, per caller

`scratchpad/f11/BoxCallers.java` asserts nothing and prints. Every value is
inside every cache's bound (`int 7`, `char 'a'`, `byte 3`, `short 9`, `long 5`,
`true`), so a `fresh` can only mean "this path allocates". `CANON` = `==` the
result of `X.valueOf(v)`.

```
== 1. Field.get  (instance) ==
Field.get int 7                                CANON  (equals=true)
Field.get char 'a'                             CANON  (equals=true)
Field.get byte 3                               CANON  (equals=true)
Field.get short 9                              CANON  (equals=true)
Field.get boolean                              CANON  (equals=true)
Field.get long 5                               CANON  (equals=true)
Field.get float 1f                             fresh  (equals=true)
Field.get double 1d                            fresh  (equals=true)
== 1b. Field.get (static) ==
Field.get static int 7                         CANON  (equals=true)
Field.get static char 'a'                      CANON  (equals=true)
Field.get static bool                          CANON  (equals=true)
== 1c. Field.get OUTSIDE the cache bound ==
Field.get int 1000                             fresh  (equals=true)
Field.get char 200                             fresh  (equals=true)
Field.get short 1000                           fresh  (equals=true)
Field.get long 1000                            fresh  (equals=true)
== 2. Array.get / Array.getXxx-boxed ==
Array.get int[] 7                              fresh  (equals=true)
Array.get char[] 'a'                           fresh  (equals=true)
Array.get byte[] 3                             fresh  (equals=true)
Array.get short[] 9                            fresh  (equals=true)
Array.get boolean[]                            fresh  (equals=true)
Array.get long[] 5                             fresh  (equals=true)
Array.get int[] twice, self-identity   = false
== 3. Method.invoke return boxing ==
Method.invoke ()I  7                           CANON  (equals=true)
Method.invoke ()C 'a'                          CANON  (equals=true)
Method.invoke ()B  3                           CANON  (equals=true)
Method.invoke ()S  9                           CANON  (equals=true)
Method.invoke ()Z true                         CANON  (equals=true)
Method.invoke ()J  5                           CANON  (equals=true)
Method.invoke ()F  1f                          fresh  (equals=true)
Method.invoke ()D  1d                          fresh  (equals=true)
== 4. MethodHandle return adaptation (asType -> Object) ==
MH asType()Object  int                         CANON  (equals=true)
MH asType()Object  char                        CANON  (equals=true)
MH asType()Object  byte                        CANON  (equals=true)
MH asType()Object  short                       CANON  (equals=true)
MH asType()Object  bool                        CANON  (equals=true)
MH asType()Object  long                        CANON  (equals=true)
MH asType()Object  float                       fresh  (equals=true)
MH .invoke() as Object int                     CANON  (equals=true)
MH invokeWithArguments int                     CANON  (equals=true)
MH invokeWithArguments char                    CANON  (equals=true)
== 5. VarHandle get (field / array / byte-array view) ==
VarHandle.get field int                        CANON  (equals=true)
VarHandle.get field char                       CANON  (equals=true)
VarHandle.get field bool                       CANON  (equals=true)
VarHandle.getAndSet int                        CANON  (equals=true)
VarHandle.get int[] elem                       CANON  (equals=true)
VarHandle.get char[] elem                      CANON  (equals=true)
VarHandle byte[]-view int                      CANON  (equals=true)
== 6. Proxy: primitive ARGS boxed into Object[] ==
Proxy args[0] int 7                            CANON  (equals=true)
Proxy args[1] char 'a'                         CANON  (equals=true)
Proxy args[2] bool                             CANON  (equals=true)
Proxy args[3] long 5                           CANON  (equals=true)
== 7. SerializedLambda captured args ==
SerializedLambda capturedArg[0] Integer        CANON  (equals=true)
SerializedLambda capturedArg[1] Character      CANON  (equals=true)
SerializedLambda capturedArg[2] Long           CANON  (equals=true)
```

`BoxCallers2.java` — the FFM layout `VarHandle`, and the uncached arm every
canonical path keeps:

```
== FFM MemorySegment layout VarHandle ==
layout VarHandle.get JAVA_INT 7                CANON  (equals=true)
layout VarHandle.get JAVA_CHAR a               CANON  (equals=true)
layout VarHandle.get JAVA_BYTE 3               CANON  (equals=true)
layout VarHandle.get JAVA_INT 1000             fresh  (equals=true)
== the uncached arm every canonical caller must keep ==
Method.invoke ()I 1000                         fresh  (equals=true)
Method.invoke ()C 200                          fresh  (equals=true)
MH asType()Object 1000                         fresh  (equals=true)
VarHandle.get static int 1000                  fresh  (equals=true)
Field.get int 1000                             fresh  (equals=true)
== self-identity across two calls (canonical paths) ==
Method.invoke 1000 twice, self-identity = false
Field.get   1000 twice, self-identity   = false
```

`BoxCallers3.java` — the collector/varargs element shape:

```
asVarargsCollector elem int 7                  CANON  (equals=true)
asVarargsCollector elem char a                 CANON  (equals=true)
asVarargsCollector elem bool                   CANON  (equals=true)
asVarargsCollector elem long 5                 CANON  (equals=true)
asCollector elem int 7                         CANON  (equals=true)
asCollector elem char a                        CANON  (equals=true)
asCollector 1000 (out of bound)                fresh  (equals=true)
```

**Every `fresh` row above is still `.equals`-equal.** An equality-shaped
assertion passes against this defect in both directions, which is why the tests
added in §4 assert on `ObjectRef` and never on the payload alone.

## 3. The per-caller table, and the JDK's reason for the asymmetry

| reflective path | HotSpot 25 | in-tree call site | helper after this change |
|---|---|---|---|
| `Field.get` (instance + static; I J Z B S C) | **CANONICAL** | `lang_class.rs:6630` | `box_value_canonical` ✅ switched |
| `Field.get` of `float`/`double` | fresh | same | falls through to `box_value` |
| `Method.invoke` primitive return | **CANONICAL** | `lang_class.rs:9519` | `box_value_canonical` ✅ switched |
| `SerializedLambda.getCapturedArg` | **CANONICAL** | `lang_class.rs:8811` | `box_value_canonical` ✅ switched |
| MethodHandle return adaptation (`asType`/`invoke`/`invokeWithArguments`) | **CANONICAL** | `lang_invoke.rs:8742`, `10634` | still `box_value` — N1 |
| MethodHandle collector/varargs element boxing | **CANONICAL** | `lang_invoke.rs:9576`–`9579`, `10597`–`10600` | still `box_value` — N1 |
| `VarHandle.get` — instance field / static / by-name | **CANONICAL** | `lang_invoke.rs:3438`, `3455`, `3477` | still `box_value` — N1 |
| `VarHandle.get` — array element | **CANONICAL** | `lang_invoke.rs:3408`, `3497` | still `box_value` — N1 |
| `VarHandle.get` — byte-array / ByteBuffer view | **CANONICAL** | `lang_invoke.rs:3383`, `3395` | still `box_value` — N1 |
| `VarHandle` RMW result (`getAndSet` …) | **CANONICAL** | `lang_invoke.rs:3336` | still `box_value` — N1 |
| FFM layout `VarHandle.get` / `MemorySegment` | **CANONICAL** | `lang_invoke.rs:570`, `3176` | still `box_value` — N1 |
| `InvocationHandler` `args[]` | **CANONICAL** | `vm/src/vm/vm_exec.rs:20792`, `20818` (a *different* implementation) | still fresh — N2 |
| **`Array.get`** | **FRESH** | `lib.rs:40938` (inlines `alloc_wrapper`; never called `box_value`) | unchanged, correct |
| `MethodHandleNatives.getMemberVMInfo` vmindex | JDK-internal, `create`-boxed | `lang_invoke.rs:12702` | `box_value`, deliberately |
| CGLIB `MethodProxy` return | canonical *by construction* (generated `FastClass` bytecode emits `valueOf`); not directly measured — no CGLIB on this host | `cglib_enhancer.rs:4978` | still `box_value` — N1 |
| CratonVM GPU API | no oracle exists | `craton_gpu.rs:1327` | `box_value`, deliberately |

**Why the JDK diverges, and why it is behaviour rather than an accident.**
Since JDK 18 `Field.get` and `Method.invoke` run on `MethodHandle`-based
accessors (`MethodHandleIntegerFieldAccessorImpl` and friends, produced by
`MethodHandleAccessorFactory`), and their boxing step is a direct handle to
`Integer.valueOf` — so they inherit `IntegerCache` for free. `Array.get` is a
VM native, `Reflection::array_get`, which boxes with
`java_lang_boxing_object::create`: that function allocates and has never
consulted a cache. The divergence is therefore an implementation detail of the
JDK — and it is nonetheless **observable**, hence specified by behaviour, hence
not ours to unify. Both directions are now pinned by tests, and the fresh
direction is the one that no equality-shaped check can see.

## 4. The fix

All of it in `native-builtins/src/lang_class.rs`:

1. **`box_value`'s doc comment now says what the body does** — "a
   `new Integer(42)`-shaped object … it is **not** `Integer.valueOf(42)`,
   whatever this comment used to claim" — and names the HotSpot function it
   mirrors (`java_lang_boxing_object::create`) so the next reader can check the
   claim instead of trusting it. The body is untouched.
2. **`box_value_canonical`** — the cached sibling. `I J Z B S C` delegate to
   the already-registered `lang_math::native_*_value_of`; `F`, `D`, `V`,
   reference descriptors and every mismatched shape fall through to
   `box_value`. The measured table of §3 is inlined above it, including the
   two rows that forbid over-caching.
3. Three call sites switched, each with the measurement that justifies it in a
   comment beside it: `Field.get`, `Method.invoke`'s return, `SerializedLambda`'s
   `capturedArgs`.

Two details where this deviates from the shape N1 sketched, both load-bearing:

* **The descriptor arm matches on the `Value` variant too.**
  `native_long_value_of` reads `Some(Value::Long(v))` and **defaults to 0** for
  anything else, and a raw `long` field slot can legitimately present as a
  compact `Value::Int` — `native_wrapper_long_value` exists precisely to widen
  that shape. Routing `("J", Value::Int(5))` into it would have returned the
  *cached* `Long.valueOf(0)` for a field holding 5: an identity bug converted
  into a wrong **answer**. This is the `[default=wrong write]` shape and it has
  its own test.
* **A failed or empty native result falls back to `box_value`, never to
  `Value::Object(None)`.** N1's sketch was
  `.ok().flatten().unwrap_or(Value::Object(None))`; mapping a boxing failure
  onto `null` is the exact defect already recorded above `create_method_object`
  in this same file ("`box_value` turned every reference return into `null`").

Five Rust tests were added to `lang_class.rs`'s test module:

* the six cached descriptors are canonical through the sibling — **and equal to
  what the `valueOf` native itself returns**, so a private-but-self-consistent
  second cache fails;
* **negative control**: `box_value` must still be `assert_ne!` for the very
  same six values. This is the test that fails when a later lane "simplifies"
  the pair into one function;
* `F`/`D` are `assert_ne!`, and each cached type's out-of-bound arm is
  `assert_ne!` (`Byte` is absent from that list on purpose — `ByteCache` has no
  fresh arm at all);
* `("J", Value::Int(5))` must come back carrying **5**, not the cached 0;
* a **source witness** that the three call sites still take the sibling. The
  behavioural tests all exercise the helpers and would every one of them still
  pass if a call site were reverted — which is the whole defect. Its needles
  are assembled with `format!` at runtime: spelled as literals they would match
  the test's own source text, since the file being searched is that file.

Each test claims its own `vm_identity` (`0x5f11`..`0x5f14`); the caches are
process-global and the mock default is `0`, shared with every other test in the
suite.

## 5. GC: no new storage, and how the pairing was verified

The split adds **no new cached storage**. It routes additional callers into the
six caches that already live in `lang_math.rs`. Verified mechanically rather
than by reading:

```
$ awk '/pub fn gc_scan_value_of_cache_roots/,/^}/'  lang_math.rs | grep -o 'scan_one_cache([a-z_]*'  | sort
$ awk '/pub fn gc_update_value_of_cache_refs/,/^}/' lang_math.rs | grep -o 'update_one_cache([a-z_]*' | sort
$ diff  ->  (no output)
SCAN/REMAP SETS IDENTICAL:
boolean_cache byte_cache character_cache integer_cache long_cache short_cache
```

and the two hooks are registered together, not independently:
`vm/src/memory/native_roots.rs:220-225` wraps them as a single
`VmRootSource { name, scan, remap }`, so a cache cannot be rooted without also
being remapped **at the registration level**. A cache that is rooted but not
remapped is a use-after-move, invisible under a non-moving collector, and
canonical wrapper instances are by construction the longest-lived objects in
the heap.

One consequence worth stating: canonical instances now reach more places. The
sibling can only ever install an instance whose payload equals its cache index
(the natives build the object from the value they were handed and only cache
in-bound values), so **it cannot poison a cache slot** — which is why §6.1 can
claim `boxid` is untouched rather than merely unaffected-so-far.

## 6. What should move, PREDICTED

### 6.1 `RJdkIntrinsics3 --only=boxid` — **zero of 69 rows**

`boxid` is 69 checks (counted: 69 `ck*` calls between `static void boxid()` and
`sectionEnd("boxid", 69)` — the file's own tripwire and the count agree). It is
`FAMILIES[1]`, F1-1 predicts it aborts at **#9** today and that #10–#69 have
never executed.

**None of the 69 is affected by this change, and the argument is mechanical:
`RJdkIntrinsics3.java` imports no `java.lang.reflect` type at all** (33
imports, listed, none reflective) and every cache row calls `X.valueOf`
directly. No row can reach `box_value` or its sibling. The family's fate is
entirely F1-1's; this lane neither advances nor endangers it. Per §5 the
sibling cannot write a wrong-valued cache entry, so it cannot perturb #1–#16
indirectly either.

### 6.2 Everything else — predicted **no verdict change anywhere**

The only observable difference this change makes is object **identity**, and
the regression suite asserts reflective boxing identity **nowhere**:

* `java.lang.reflect.Array.get` appears in no `--only` family (the two files
  matching `Array.get(` are `AtomicIntegerArray`/`AtomicReferenceArray` rows);
* every `Field.get`/`Method.invoke` row that uses `==` compares through an
  **unboxing** conversion — `((Integer) max.get(null)) == Integer.MAX_VALUE`
  (`RJdkFieldModule:197`), `((Integer) f.get(h)) == 9` (`RJdkSqlPackage:272`),
  `((Integer) bt.get(e)) == 11` (`RJdkFieldModule:206`) — so they are
  identity-neutral in both directions;
* the two reference-identity rows on reflective reads (`RJdkFieldModule:190`
  `out.get(null) == System.out`, `RJdkSqlPackage:227`) are on **reference**
  fields, which pass through `box_value`'s catch-all untouched.

So the predicted effect is: conformance to HotSpot on a surface nothing
measures, minus one wrapper allocation per in-bound reflective read. If a row
*does* move, it is a row this section says cannot — start there.

### 6.3 One behaviour change that is not identity

`box_value(v, "Z")` stored the raw slot verbatim, so a `boolean` field whose
slot held `Int(5)` produced a `Boolean` carrying 5. The canonical route
normalises through `native_boolean_value_of`, which is `val != 0` and returns
`Boolean.TRUE`. That matches HotSpot (whose `Field.get` on a `boolean` cannot
produce anything but `TRUE`/`FALSE`) and is strictly better, but it *is* a
change and it is here so that a bisect lands on this sentence.

## 7. NOMINATIONS

### N1 — `native-builtins/src/lang_invoke.rs`: 21 call sites, all measured canonical, none switched

**Evidence:** §2 and §3. Every MethodHandle/VarHandle/FFM shape measured on
HotSpot 25 returns the canonical box; all 21 sites still call `box_value`.

Not done here because this lane's write scope is `lang_class.rs` and nine
sibling lanes are live in the same worktree. The work is mechanical now that
the sibling exists and is `pub(crate)`: at each site listed in §3 replace
`box_value(` with `box_value_canonical(`. **Do not** switch
`lang_invoke.rs:12702` (`getMemberVMInfo`'s vmindex — a JDK-internal Object[]
slot whose HotSpot counterpart is `create`-boxed) or `craton_gpu.rs:1327` (a
CratonVM-only API with no oracle). `cglib_enhancer.rs:4978` should switch:
CGLIB's generated `FastClass` boxes returns with `valueOf` bytecode, so a real
CGLIB run on HotSpot is canonical — though that one is reasoned, not measured,
because there is no CGLIB on this host.

### N2 — `vm/src/vm/vm_exec.rs`: a fourth boxing implementation, and its `Boolean` arm is the Xerces shape

`proxy_box_value_for_desc` / `proxy_box_value` (lines 20792, 20818) box an
`InvocationHandler`'s `args[]` and allocate unconditionally. Measured on
HotSpot: **all four** proxy argument rows are canonical (§2 block 6).

The `Z` arm is the more interesting half: it does `alloc_object` +
`set_field(0)`, so a proxy handler's `args[i]` is **not** `Boolean.TRUE`. That
is exactly the failure `native_boolean_value_of`'s own comment documents for
Xerces' `XML11Configuration.configurePipeline()`
(`fFeatures.get(…) == Boolean.TRUE`) — the same bug, one layer over, in a file
that cannot see the fix. Any handler that identity-tests a boxed argument
against `Boolean.TRUE`, or against a small `Integer`, disagrees with HotSpot
here.

`vm_exec.rs` cannot call `lang_class::box_value_canonical` (wrong crate
direction), so this is a `SharedVm`-side fix: either resolve
`java/lang/Boolean.TRUE`/`FALSE` the way the native does, or route the proxy
path through the native registry. Sizing and ownership are the picking lane's.

### N3 — the reflective boxing identity surface is asserted by nothing

§6.2: there is no check anywhere in `regression-suite/src` that asks whether a
reflective read returns the canonical box — in **either** direction. This lane's
change and its exact inverse both pass the whole suite. Suggested rows, in
`RJdkIntrinsics3` (it would need `java.lang.reflect` imports, so a new family
`reflbox` rather than an extension of `boxid`, whose "no reflection at all"
property §6.1 leans on and should be preserved):

```java
        // Field.get boxes through a MethodHandle accessor -> X.valueOf, so it
        // is CANONICAL; Array.get is Reflection::array_get -> create(), so it
        // is FRESH. Both measured on HotSpot 25.0.3+9. The second row is the
        // one that fails if a VM "unifies" its two boxing helpers, and every
        // row here is .equals-equal, so equality cannot stand in for it.
        ckB("reflbox:Field.get(char) is Character.valueOf",
                F_CHAR.get(holder) == Character.valueOf('a'), true);
        ckB("reflbox:Array.get(char[]) is NOT Character.valueOf",
                Array.get(new char[]{'a'}, 0) == Character.valueOf('a'), false);
        ckB("reflbox:Method.invoke()I is Integer.valueOf",
                M_INT.invoke(null) == Integer.valueOf(7), true);
        ckB("reflbox:Field.get(float) is NOT Float.valueOf",
                F_FLOAT.get(holder) == Float.valueOf(1f), false);
```

The operands must come from non-`static final` sources or `javac` folds them.

### N4 — `lib.rs::native_array_get` should say *why* it allocates

`lib.rs:40938`–`41022` is eight copy-pasted `alloc_wrapper` blocks whose comment
explains only the wrapper **type** ("for char[] we must return Character"). It
does not say that allocating is itself the contract. It is one sentence away
from a lane "deduplicating" it onto the cached sibling and silently breaking
`Array.get(new int[]{7},0) == Integer.valueOf(7) == false`. One line, pointing
at `Reflection::array_get` and at this record. This lane did not edit `lib.rs`
because it is owned elsewhere this session.

## 8. Residuals

* The three switched sites now hand out shared instances. Wrappers are
  immutable *in Java*, but anything Rust-side that took a `Field.get`/
  `Method.invoke` result and wrote its slot 0 would now corrupt a cache
  process-wide. Searched: outside `lang_math.rs`/`lang_class.rs` the only
  in-tree writers of a wrapper's slot 0 are `lib.rs::native_array_get` (which
  writes objects it just allocated), `vm_exec.rs`'s proxy boxing (same) and
  `xml_xerces.rs:419` (same). No consumer-side mutation was found.
* `box_value_canonical` runs `<clinit>` (through `alloc_wrapper` /
  `ensure_class_initialized`) exactly as `box_value` already did, so it adds no
  new GC or reentrancy hazard at any of the three sites; each site's existing
  pins were left as they were.
* Nothing in this record claims a CratonVM behaviour was observed. §6 is a
  prediction, written so that one run can falsify it.
