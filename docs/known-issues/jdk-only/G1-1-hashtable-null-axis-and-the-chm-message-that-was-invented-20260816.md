# G1-1 — the Hashtable null axis, and the ConcurrentHashMap message that was invented

> **RECONCILED 2026-08-17 (lane G40) — the provenance premise "no JDK source was
> read" was avoidable.** `C:\craton\jdk25src` is indeed absent, and this record
> is right about that. But the JDK's sources ship with the oracle itself, at
> `$JAVA_HOME/lib/src.zip` (52,462,198 bytes) — the sources of the exact
> HotSpot 25.0.3+9-LTS build used here. `unzip -p "$JAVA_HOME/lib/src.zip"
> java.base/java/util/Hashtable.java`. Nothing measured in this record is
> invalidated; the point is that the `ConcurrentHashMap` message this record
> calls invented is **transcribable in two minutes** rather than derivable from
> behaviour. See `INDEX.md` §B.3.

**Status:** ORACLE MEASURED / **CRATONVM PREDICTED**.
**Provenance:** every number in §2 and §3 was **MEASURED on HotSpot
25.0.3+9-LTS** (`$JAVA_HOME`, single-file source mode) on 2026-08-16. **Not one
number in this record was measured on CratonVM.** This lane could not build and
could not run the VM — the orchestrator owns the build and the tree is
mid-merge. Every "after" claim in §4 and §5 is therefore a **PREDICTION**, in
the exact sense HANDOFF-20260814 §2 warns about. Treat it as analysis until a
binary agrees with it.

Branch `claude/jdk-only-mode-completion-1351c0`, working tree at
`c479a668a` + the wave's uncommitted lanes, 2026-08-17T02:45Z.
Probes: `scratchpad/g1/Probe.java`, `scratchpad/g1/Probe2.java` (both
reproduced verbatim in §7); raw output in `scratchpad/g1/oracle-out.txt`,
`oracle-out2.txt`. Bytecode evidence from
`javap -p -c java.util.Hashtable` / `java.util.concurrent.ConcurrentHashMap` /
`java.util.PriorityQueue` against the same JDK. `C:\craton\jdk25src` is absent
on this machine, so no JDK source was read — only behaviour and bytecode.

Picks up HANDOFF-20260814 §6 items **2** (`Hashtable`'s null axis, "7 of 9 rows
diverge", measured by F41-1 and never fixed) and **3** (`ConcurrentHashMap`'s
message, "has never been compared with the oracle").

---

## 0. The headline

* **`Hashtable`'s null rule is two rules, and they are not `Properties`' two
  rules.** Value/function first and **message-less**; key second and carrying
  HotSpot's helpful-NPE text `Cannot invoke "Object.hashCode()" because "key"
  is null`. The order is observable: `ht.put(null, null)` is message-less.
* **The two rows F41-1 counted as *matching* were matching on exception KIND
  only.** `remove(k,v)` and the two `replace` overloads already refused a null
  key — with `message: None`, where a plain `Hashtable` produces the
  `hashCode` text. Same fabrication class F41-1 §3 found in `Properties`, one
  layer down.
* **`Properties.remove(k, null)` does not throw at all** — it answers `false`.
  The shared helper threw. This is a **newly measured** `Properties` row, i.e.
  the 15-of-15 in F41-1 §3 did not cover it.
* **`ConcurrentHashMap` never carried any of the messages this file gave it.**
  Ten fabricated strings across ten functions. The write path throws a bare
  `new NullPointerException()`; the lookup path throws HotSpot's helpful-NPE,
  which is *not* the same thing and is not message-less either.
* **`PriorityBlockingQueue`'s string was the javadoc sentence, not a message.**
  `PriorityQueue`'s twin (`ad_refuse_null`) had already got this right and said
  so in its own doc comment; the PBQ copy drifted.
* One row would have been broken by any blanket rule:
  **`Hashtable.merge(k, null, f)` succeeds** where `HashMap.merge(k, null, f)`
  throws. `Hashtable.merge` `requireNonNull`s only its remapping function.

---

## 1. What the "9 rows" were, and why 9 was the wrong denominator

F41-1 §5 records `java/util/Hashtable` diverging on **7 of 9** rows and names
them: `get`, `getOrDefault`, `containsKey`, `containsValue`, `put` both ways,
`remove`. The two that "matched" are the ones the `Properties` fix had already
routed through `is_hashtable_receiver` — `remove(k,v)` and `replace`.

Nine rows is a sample. The reachable `Hashtable` null surface — the cells a
`java/util/Hashtable` receiver can actually reach in this VM, i.e. the methods
`register_properties_natives` registers under `ht` — is **twelve**:

| # | cell | served by |
|---|---|---|
| 1 | `get(null)` | `native_map_get` |
| 2 | `getOrDefault(null, d)` | real JDK bytecode -> `native_map_get` |
| 3 | `containsKey(null)` | `native_map_contains_key` |
| 4 | `containsValue(null)` | `native_map_contains_value` |
| 5 | `put(null, v)` | `native_map_put` |
| 6 | `put(k, null)` | `native_map_put` |
| 7 | `remove(null)` | `native_map_remove` |
| 8 | `putAll(null)` | `native_map_put_all` |
| 9 | `remove(k, v)`, 3 null shapes | `native_map_remove_kv` |
| 10 | `replace(k, v)`, 2 null shapes | `native_map_replace` |
| 11 | `replace(k, o, n)`, 3 null shapes | `native_map_replace_kv` |
| 12 | `contains(null)` | real JDK bytecode (not registered) |

Rows 8 and 12 are outside F41-1's nine. Row 8 (`putAll(null)`) was a **silent
no-op** on every receiver, `HashMap` included. Rows 9–11 are the ones that
counted as matching.

`putIfAbsent`, `merge`, `compute*`, `replaceAll` and `forEach` are **not**
registered for `java/util/Hashtable` and the `java/util/Map` interface door
registers only `size`/`forEach`/`get`/`put`/`containsKey`/`keySet`/`values`/
`entrySet` — so a `Hashtable` receiver cannot reach `native_map_merge` or the
`compute` family at all. That is why nothing below touches them, and it is
also why the `merge` surprise in §2 costs no code: `Hashtable.merge` runs real
JDK bytecode today.

---

## 2. The oracle table — `Map` family, MEASURED

`java -version` -> `openjdk 25.0.3 2026-04-21 LTS / Temurin-25.0.3+9`.
`<NO-MESSAGE>` means `getMessage()` returned **`null`**, printed distinctly
from `""` on purpose. `HASHCODE-NPE` abbreviates the exact string
`Cannot invoke "Object.hashCode()" because "key" is null`.

| cell | `HashMap` | `LinkedHashMap` | `Hashtable` | `Properties` | `ConcurrentHashMap` |
|---|---|---|---|---|---|
| `get(null)` | `null` | `null` | NPE HASHCODE-NPE | NPE HASHCODE-NPE | NPE HASHCODE-NPE |
| `getOrDefault(null,d)` | `"D"` | `"D"` | NPE HASHCODE-NPE | NPE HASHCODE-NPE | NPE HASHCODE-NPE |
| `containsKey(null)` | `false` | `false` | NPE HASHCODE-NPE | NPE HASHCODE-NPE | NPE HASHCODE-NPE |
| `containsValue(null)` | `false` | `false` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `put(null,v)` | `null` | `null` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `put(k,null)` | `null` | `null` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `put(null,null)` | `null` | — | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `remove(null)` | `null` | `null` | NPE HASHCODE-NPE | NPE HASHCODE-NPE | NPE HASHCODE-NPE |
| `remove(null,v)` | `false` | `false` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `remove(k,null)` | `false` | `false` | NPE `<NO-MESSAGE>` | **`false`** | **`false`** |
| `remove(null,null)` | `false` | — | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `putIfAbsent(null,v)` | `null` | `null` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `putIfAbsent(k,null)` | `null` | `null` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(null,v)` | `null` | `null` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(k,null)` | `"v"` | `"v"` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(null,null)` | `null` | — | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(null,o,n)` | `false` | `false` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(k,null,n)` | `false` | `false` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replace(k,o,null)` | `true` | `true` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `computeIfAbsent(null,f)` | `"computed"` | `"computed"` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `computeIfAbsent(k,null)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `computeIfPresent(null,f)` | `null` | `null` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `compute(null,f)` | `"bicomputed"` | `"bicomputed"` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `compute(k,null)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `merge(null,v,f)` | `"v"` | `"v"` | NPE HASHCODE-NPE | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `merge(k,null,f)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | **`"bicomputed"`** | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `merge(k,v,null)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `putAll(null)` | NPE `size()`/`m` | NPE `size()`/`m` | NPE **`entrySet()`/`t`** | NPE `size()`/`m` | NPE `size()`/`m` |
| `forEach(null)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `replaceAll(null)` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` |
| `equals(null)` | `false` | `false` | `false` | `false` | `false` |
| `contains(null)` | n/a | n/a | NPE `<NO-MESSAGE>` | NPE `<NO-MESSAGE>` | n/a |

The two `putAll` strings in full, transcribed:

```text
Cannot invoke "java.util.Map.size()" because "m" is null
Cannot invoke "java.util.Map.entrySet()" because "t" is null
```

Calibration rows, so the three NPE shapes are not confused with each other:

```text
calibration | new NPE().getMessage()       | java.lang.NullPointerException msg=<NO-MESSAGE>
calibration | Objects.requireNonNull(null) | java.lang.NullPointerException msg=<NO-MESSAGE>
calibration | ((Object)null).hashCode()    | java.lang.NullPointerException
                msg=<<Cannot invoke "Object.hashCode()" because "<local0>" is null>>
```

The third one is the point: the helpful-NPE text **names the local**, so it
is a property of the JDK method that produced it and cannot be derived from
the call. It reads `"key"` in every `Hashtable` and `ConcurrentHashMap` method
that produces it because that is what the JDK calls its parameter. Transcribe;
do not compose.

### 2a. `Hashtable`'s rule, stated

* **RULE V** — a null **value or function** is refused **first**, by
  `Objects.requireNonNull(...)` (`put`, `putIfAbsent`, `replace` x2,
  `remove(k,v)`, `computeIfAbsent`, `compute`, `computeIfPresent`, `merge`'s
  function, `forEach`, `replaceAll`) or by a literal
  `throw new NullPointerException()` (`contains`, and `containsValue` via it).
  **No message.** `containsValue(null)` refuses even on an empty table.
* **RULE K** — only once RULE V passes does a null **key** die, on the bucket
  walk's own `int hash = key.hashCode();`. Helpful-NPE, HASHCODE-NPE text.

The order is observable, which is what makes it a rule and not a detail:
`put(null,null)`, `remove(null,null)`, `replace(null,null)`,
`replace(null,null,null)`, `putIfAbsent(null,null)`, `compute(null,null)`,
`merge(null,v,null)` are all `<NO-MESSAGE>`, while `merge(null,null,f)` —
where RULE V has nothing to catch, because `merge` does not check its value —
is HASHCODE-NPE.

### 2b. `Properties` is a third contract, not a special case of the second

Its write path delegates to a side `ConcurrentHashMap`, so:

* every write refusal is `<NO-MESSAGE>` (no `hashCode` text) — including
  `put(null, v)`, where a plain `Hashtable` gives HASHCODE-NPE;
* `props.remove(k, null)` **answers `false`**, exactly like
  `chm.remove(k, null)`, because `ConcurrentHashMap.remove(Object,Object)` is
  `if (key == null) throw new NullPointerException(); return value != null && ...`;
* only the *read* path (`get`, `getOrDefault`, `containsKey`, `remove(k)`,
  `getProperty` x2) still reaches `Hashtable.get` and gives HASHCODE-NPE —
  which is F41-1 §3's two-rule finding, confirmed here independently.

`putAll(null)` is a fourth message for `Hashtable` and the *`HashMap`* message
for `Properties`, because `Properties.putAll` is not `Hashtable.putAll`.

---

## 3. The `ConcurrentHashMap` messages — MEASURED, and there were ten

HANDOFF §6 item 3 named one string. There were **ten**, spread over **ten**
functions, none of which had ever been compared with the oracle.

The oracle has exactly two answers for `ConcurrentHashMap`, and the split is
lookup-vs-write, not key-vs-value:

* **lookup path** (`get`, `getOrDefault`, `containsKey`, `remove(Object)`) —
  no explicit null test at all; each opens `int h = spread(key.hashCode());`,
  so the answer is the helpful-NPE **HASHCODE-NPE**, *with* a message;
* **write path** (`put`, `putIfAbsent`, `replace` x2, `merge`, `compute*`,
  `remove(Object,Object)`) — an explicit `throw new NullPointerException()`,
  **no message**.

`putVal`'s prologue, from `javap -p -c java.util.concurrent.ConcurrentHashMap`:

```text
final V putVal(K, V, boolean);
   0: aload_1
   1: ifnull        8
   4: aload_2
   5: ifnonnull     16
   8: new           #162   // class java/lang/NullPointerException
  11: dup
  12: invokespecial #164   // Method java/lang/NullPointerException."<init>":()V
  15: athrow
  16: aload_1
  17: invokevirtual #122   // Method java/lang/Object.hashCode:()I
```

The **no-arg** constructor at 12 is the whole finding: there is no message to
carry, and `remove(Object,Object)` has the same shape with the key test alone.

The ten fabricated strings, every one of which appears **nowhere in the JDK**:

| site (pre-edit line) | fabricated string | oracle |
|---|---|---|
| `chm_reject_null_key` :49536 | `ConcurrentHashMap does not permit null keys` | HASHCODE-NPE (lookup path) |
| `native_chm_put` :50138 | `ConcurrentHashMap does not permit null keys or values` | `<NO-MESSAGE>` |
| `native_chm_put_if_absent` :50210 | `ConcurrentHashMap does not permit null keys or values` | `<NO-MESSAGE>` |
| `native_chm_compute_if_absent` | `ConcurrentHashMap.computeIfAbsent: null key or mappingFunction` | `<NO-MESSAGE>` |
| `native_chm_compute` | `ConcurrentHashMap.compute: null key or remappingFunction` | `<NO-MESSAGE>` |
| `native_chm_compute_if_present` | `ConcurrentHashMap.computeIfPresent: null key or remappingFunction` | `<NO-MESSAGE>` |
| `native_chm_merge` | `ConcurrentHashMap.merge: null key, value, or remappingFunction` | `<NO-MESSAGE>` |
| `native_chm_replace` | `ConcurrentHashMap.replace: null key or value` | `<NO-MESSAGE>` |
| `native_chm_replace_kv` | `ConcurrentHashMap.replace(k,old,new): nulls not permitted` | `<NO-MESSAGE>` |
| `native_chm_key_set_view` | `ConcurrentHashMap.keySet(null)` | `<NO-MESSAGE>` |

`native_chm_remove_kv` was the one real *routing* error rather than a text
error: it called `chm_reject_null_key`, i.e. the lookup-path helper, on a
**write-path** method. Oracle: `chm.remove(null, v)` is `<NO-MESSAGE>`.

### 3a. The two neighbours the handoff asked about

* `:39822` (actually **`:59822`** — the handoff's line number was off by
  20,000; the string is in `pbq_reject_null_element`)
  `"PriorityBlockingQueue does not permit null elements"` — **FABRICATED**.
  Oracle: `<NO-MESSAGE>` for both `PriorityQueue` and `PriorityBlockingQueue`,
  and `javap -p -c java.util.PriorityQueue` shows `offer` compiling to the same
  no-arg `NullPointerException` constructor. The string is the **javadoc
  sentence** ("This queue does not permit null elements"), which is a
  specification, not a message.
* `:39722`'s `PriorityQueue` comment — **not a defect**. It quotes the javadoc
  in a comment and then calls `ad_refuse_null`, whose own doc comment already
  says *"HotSpot's is message-less … so inventing one would itself be a
  divergence"* and whose body is `message: None`. The `ArrayDeque` sibling got
  this right; only the `PriorityBlockingQueue` copy drifted. A fourth copy of
  the same rule is exactly the drift HANDOFF §5 warns about.

Oracle rows:

```text
java.util.PriorityQueue    | add(null)       | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.PriorityQueue    | offer(null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.PriorityQueue    | contains(null)  | OK -> false
java.util.PriorityQueue    | remove(null)    | OK -> false
j.u.c.PriorityBlockingQueue| add(null)       | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.PriorityBlockingQueue| offer(null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.PriorityBlockingQueue| contains(null)  | OK -> false
j.u.c.PriorityBlockingQueue| remove(null)    | OK -> false
```

Note `contains(null)` and `remove(null)` answer `false` on both — a blanket
"PriorityQueue refuses null" would have been wrong for half the surface.

---

## 4. What was changed — PREDICTED effect, all of it

All edits are in `native-collections/src/lib.rs`. No other Rust file was
touched. Nothing here has been compiled or run.

### 4.1 One receiver predicate, two rule helpers

`is_plain_hashtable_receiver` — `Hashtable` ancestry with **`Properties`
excluded**, delegating to the existing memoized `CF_HASHTABLE_LAYOUT` bit that
`uses_native_hashtable_layout` reads. The existing `is_hashtable_receiver`
(Properties **included**) stays, and every contract below asks both, in that
order. This is the point the handoff made: `Hashtable` needs
**receiver-routing**, and it needs *two* receiver classes, not one.

`ht_reject_null_key` (RULE K, HASHCODE-NPE) and `ht_reject_null_value`
(RULE V, message-less) take `Option<&Value>` — `args.get(n)` — so a **missing**
argument stays a malformed-call no-op and only an **explicitly passed** null
throws. That distinction is `native_map_merge`'s existing precedent.

`bare_npe()` mirrors the existing `unsupported_op()` helper.

### 4.2 The generic `Map` natives, receiver-routed

| body | added | predicted `Hashtable` cell | `HashMap` unchanged? |
|---|---|---|---|
| `native_map_get` | RULE K | `get(null)` -> HASHCODE-NPE; also fixes `getOrDefault` (real JDK body calls this native) | yes — guard is receiver-gated |
| `native_map_contains_key` | RULE K | `containsKey(null)` -> HASHCODE-NPE | yes |
| `native_map_remove` | RULE K | `remove(null)` -> HASHCODE-NPE | yes |
| `native_map_contains_value` | RULE V | `containsValue(null)` -> `<NO-MESSAGE>` | yes |
| `native_map_put_evict` | RULE V **then** RULE K | `put(k,null)` -> `<NO-MESSAGE>`; `put(null,v)` -> HASHCODE-NPE; `put(null,null)` -> `<NO-MESSAGE>` | yes |
| `native_map_put_all` | null-source refusal, receiver-routed message | `putAll(null)` -> `entrySet()`/`t` | **no** — `HashMap.putAll(null)` changes from a silent no-op to the measured `size()`/`m` NPE |

The last row is the one deliberate widening beyond `Hashtable`: the body was
silently doing nothing for **every** receiver, and both messages are
transcribed from the oracle in §2. Called out because it is the only change
here that can alter a `HashMap` path.

### 4.3 The three conditional mutators, split by receiver

`map_kv_reject_null_for_hashtable` and `map_k_reject_null_for_hashtable` are
replaced by three per-method contracts, because the three JDK methods do not
agree with each other and the two receivers do not agree either:

| method | plain `Hashtable` | `Properties` |
|---|---|---|
| `remove(k,v)` | value (`<NO-MESSAGE>`) then key (HASHCODE-NPE) | key only (`<NO-MESSAGE>`); **null value falls through and answers `false`** |
| `replace(k,v)` | value then key | either (`<NO-MESSAGE>`) |
| `replace(k,o,n)` | old, new, then key | any (`<NO-MESSAGE>`) |

Two behaviour changes fall out of this beyond "add the message":

* the key arm's message changes from `None` to HASHCODE-NPE **for plain
  `Hashtable` only** — the two rows F41-1 scored as matching;
* the value check on `Properties.remove(k,v)` is **removed**, because the
  oracle answers `false`. See §6: this is the one row that contradicts a
  green fixture's possible expectation.

The old code also checked **key before value** in both `replace` overloads.
That is backwards for `Hashtable` and observable via `replace(null, null)`.

### 4.4 ConcurrentHashMap and PriorityBlockingQueue

* `chm_reject_null_key` keeps its four **lookup-path** callers (`get`,
  `getOrDefault`, `containsKey`, `remove(Object)`) and now emits the shared
  `HASHTABLE_NULL_KEY_MSG` constant — the same text, for the same reason, as
  `java.util.Hashtable`.
* new `chm_bare_npe_on_null` for the write path; `native_chm_put`,
  `native_chm_put_if_absent` and `native_chm_replace` call it.
* `native_chm_remove_kv` **stops calling the lookup-path helper** and throws
  the bare NPE its JDK body throws.
* `native_chm_compute_if_absent`, `native_chm_compute`,
  `native_chm_compute_if_present`, `native_chm_merge`,
  `native_chm_replace_kv` and `native_chm_key_set_view` -> `bare_npe()`.
  Conditions unchanged; only the invented text is gone.
* `pbq_reject_null_element` -> `bare_npe()`, with the oracle rows and the
  `ad_refuse_null` precedent written into its doc comment.

No behaviour outside the message text changes for CHM except
`remove(Object,Object)`'s message.

---

## 5. Cells this record does NOT fix, and why

All MEASURED above; none touched.

* **`HashMap`/`LinkedHashMap` compute-family null *function*.**
  `native_map_compute_if_absent` / `compute` / `compute_if_present` return
  `Ok(Some(null))` for an explicitly null function; the oracle throws a bare
  NPE. Same for `native_map_for_each(null)` and
  `native_map_replace_all(null)`. Not fixed: a `Hashtable` receiver cannot
  reach these bodies (§1), so it is a `HashMap`-family defect on a different
  axis, and widening this lane's blast radius into paths `RJdkBridge1` and
  `RJdkIntrinsics3` currently exercise is exactly what HANDOFF §5's first trap
  is about. **Recommended as a small standalone follow-up** — the fix is one
  `matches!(args.get(n), Some(Value::Object(None)))` per site and the oracle
  answer is uniform (`<NO-MESSAGE>`) across all five receivers.
* **`Hashtable.merge(k, null, f)`.** Succeeds on the oracle. Unregistered here,
  so real JDK bytecode runs and is presumed right. Written down so the next
  lane does not "fix" `native_map_merge` into refusing it — which would be
  correct for `HashMap` and wrong for `Hashtable`, and the shared body cannot
  tell them apart without the receiver test §4.1 adds.
* **`Hashtable.putIfAbsent` / `compute*` / `replaceAll` over real JDK
  bytecode.** Their bodies walk `Hashtable`'s own `table[]`, which this VM
  populates with `HashMap$Node`s — the hazard already documented at
  `register_map_conditional_mutators`. Their null behaviour is probably right
  by accident; their *entry* behaviour is a separate, larger question.
* **`Properties`' unregistered null surface.** `Properties.merge`,
  `compute*`, `putIfAbsent` and `replaceAll` are registered by neither this
  file nor `properties_sidetable.rs`. Out of lane.

---

## 6. What the orchestrator must check at build time

1. **Compilation.** New symbols, all in `native-collections/src/lib.rs`:
   `HASHTABLE_NULL_KEY_MSG`, `is_plain_hashtable_receiver`,
   `ht_reject_null_key`, `ht_reject_null_value`, `bare_npe`,
   `map_remove_kv_null_contract`, `map_replace_kv_null_contract`,
   `map_replace3_null_contract`, `chm_bare_npe_on_null`. All are used;
   `map_kv_reject_null_for_hashtable` and `map_k_reject_null_for_hashtable`
   are **deleted** and had no callers outside this file (checked repo-wide).
   The helpers take `&dyn NativeContext` and are called from `&mut dyn`
   contexts, which is the same reborrow every `is_*_receiver` call site
   already relies on.
2. **`Properties.remove(k, null)`.** This lane **removed** a refusal that
   F41-1's 15-of-15 sweep may or may not have covered — its row list names
   `remove(k,v)`, which the probe most likely exercised with a null *key*.
   The oracle is unambiguous (`OK -> false`), so a fixture asserting a throw is
   asserting the wrong thing; but it is the single row most likely to flip a
   currently-green `Properties` assertion, and it should be looked at first if
   one does.
3. **`HashMap.putAll(null)`** now throws where it silently no-op'd. Any
   internal caller that relied on the tolerance would surface here;
   `native_map_put_all` has no in-repo callers other than its three
   registrations (`HashMap`, `Hashtable`, `Properties`), so the risk is
   confined to Java code that actually passes null.
4. **`--dump-native-registry` before believing any of §4.** Per HANDOFF §4,
   a line number proves where a body is, never that it runs. This record
   argues from *registration sites* — `register_properties_natives`'s `ht`
   block, `register_hashmap_natives`, `register_map_conditional_mutators`, and
   the `java/util/Map` interface door — not from a dump. If `owns_slot` says
   something else owns `java/util/Hashtable.get`, §1's table is wrong and the
   fix lands in a body with `invocations=0`, which is failure mode #1 in
   HANDOFF §5.
5. **Re-run the probes as a differential.** `Probe.java` and `Probe2.java`
   below are ASCII-only in every printed label, deliberately (HANDOFF §7's
   em-dash incident). Run both on HotSpot and on CratonVM, `tr -d '\r'`, diff.
   That is the only thing that turns this record from PREDICTED to MEASURED.

---

## 7. The probes

Both were run as `"$JAVA_HOME/bin/java" Probe.java` (single-file source mode,
no `javac`). `JAVA_HOME=C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`.

### 7.1 `scratchpad/g1/Probe.java` — the family sweep

```java
import java.util.*;
import java.util.concurrent.*;
import java.util.function.*;

public class Probe {

    interface Cell { Object run() throws Throwable; }

    static String cls = "?";

    static void row(String label, Cell c) {
        String out;
        try {
            Object r = c.run();
            out = "OK -> " + describe(r);
        } catch (Throwable t) {
            String m = t.getMessage();
            String ms = (m == null) ? "<NO-MESSAGE>" : "<<" + m + ">>";
            out = t.getClass().getName() + " msg=" + ms;
        }
        System.out.println(pad(cls, 24) + "| " + pad(label, 34) + "| " + out);
    }

    static String pad(String s, int n) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < n) b.append(' ');
        return b.toString();
    }

    static String describe(Object o) {
        if (o == null) return "null";
        if (o instanceof String) return "\"" + o + "\"";
        return String.valueOf(o);
    }

    static final Function<Object,Object> F1 = k -> "computed";
    static final BiFunction<Object,Object,Object> BF = (a,b) -> "bicomputed";

    @SuppressWarnings("unchecked")
    static void mapFamily(String name, Supplier<Map<Object,Object>> mk) {
        cls = name;
        // read path
        row("get(null)",                     () -> mk.get().get(null));
        row("getOrDefault(null,d)",          () -> mk.get().getOrDefault(null, "D"));
        row("containsKey(null)",             () -> mk.get().containsKey(null));
        row("containsValue(null)",           () -> mk.get().containsValue(null));
        // write path
        row("put(null,v)",                   () -> mk.get().put(null, "v"));
        row("put(k,null)",                   () -> mk.get().put("k2", null));
        row("remove(null)",                  () -> mk.get().remove(null));
        row("remove(null,v)",                () -> mk.get().remove(null, "v"));
        row("remove(k,null)",                () -> mk.get().remove("k", null));
        row("putIfAbsent(null,v)",           () -> mk.get().putIfAbsent(null, "v"));
        row("putIfAbsent(k,null)",           () -> mk.get().putIfAbsent("k2", null));
        row("replace(null,v)",               () -> mk.get().replace(null, "v"));
        row("replace(k,null)",               () -> mk.get().replace("k", null));
        row("replace(null,o,n)",             () -> mk.get().replace(null, "v", "n"));
        row("replace(k,null,n)",             () -> mk.get().replace("k", null, "n"));
        row("replace(k,o,null)",             () -> mk.get().replace("k", "v", null));
        row("computeIfAbsent(null,f)",       () -> mk.get().computeIfAbsent(null, F1));
        row("computeIfAbsent(k,null)",       () -> mk.get().computeIfAbsent("k9", null));
        row("computeIfPresent(null,f)",      () -> mk.get().computeIfPresent(null, BF));
        row("computeIfPresent(k,null)",      () -> mk.get().computeIfPresent("k", null));
        row("compute(null,f)",               () -> mk.get().compute(null, BF));
        row("compute(k,null)",               () -> mk.get().compute("k", null));
        row("merge(null,v,f)",               () -> mk.get().merge(null, "v", BF));
        row("merge(k,null,f)",               () -> mk.get().merge("k", null, BF));
        row("merge(k,v,null)",               () -> mk.get().merge("k", "v", null));
        row("putAll(null)",                  () -> { mk.get().putAll(null); return "no-throw"; });
        row("forEach(null)",                 () -> { mk.get().forEach(null); return "no-throw"; });
        row("replaceAll(null)",              () -> { mk.get().replaceAll(null); return "no-throw"; });
        row("equals(null)",                  () -> mk.get().equals(null));
        // Hashtable-only legacy surface
        Map<Object,Object> probe = mk.get();
        if (probe instanceof Hashtable) {
            row("contains(null)  [Hashtable]", () -> ((Hashtable<Object,Object>) mk.get()).contains(null));
        }
        if (probe instanceof Properties) {
            row("getProperty(null)",           () -> ((Properties) mk.get()).getProperty(null));
            row("getProperty(null,d)",         () -> ((Properties) mk.get()).getProperty(null, "D"));
            row("setProperty(null,v)",         () -> ((Properties) mk.get()).setProperty(null, "v"));
            row("setProperty(k,null)",         () -> ((Properties) mk.get()).setProperty("k", null));
        }
        System.out.println();
    }

    static void queueFamily(String name, Supplier<Queue<Object>> mk) {
        cls = name;
        row("add(null)",       () -> mk.get().add(null));
        row("offer(null)",     () -> mk.get().offer(null));
        row("contains(null)",  () -> mk.get().contains(null));
        row("remove(null)",    () -> mk.get().remove(null));
        row("addAll([null])",  () -> { List<Object> l = new ArrayList<>(); l.add(null);
                                       return mk.get().addAll(l); });
        row("removeAll([null])", () -> { List<Object> l = new ArrayList<>(); l.add(null);
                                       return mk.get().removeAll(l); });
        System.out.println();
    }

    public static void main(String[] a) throws Exception {
        System.out.println("ORACLE " + System.getProperty("java.vm.name") + " "
                + System.getProperty("java.runtime.version"));
        System.out.println("CLASS                   | CELL                              | RESULT");
        System.out.println();

        mapFamily("java.util.HashMap", () -> { Map<Object,Object> m = new HashMap<>();
                                               m.put("k", "v"); return m; });
        mapFamily("java.util.LinkedHashMap", () -> { Map<Object,Object> m = new LinkedHashMap<>();
                                               m.put("k", "v"); return m; });
        mapFamily("java.util.Hashtable", () -> { Map<Object,Object> m = new Hashtable<>();
                                               m.put("k", "v"); return m; });
        mapFamily("java.util.Properties", () -> { Properties p = new Properties();
                                               p.put("k", "v"); return p; });
        mapFamily("j.u.c.ConcurrentHashMap", () -> { Map<Object,Object> m = new ConcurrentHashMap<>();
                                               m.put("k", "v"); return m; });

        queueFamily("java.util.PriorityQueue", () -> new PriorityQueue<>());
        queueFamily("j.u.c.PriorityBlockingQueue", () -> new PriorityBlockingQueue<>());

        // extra: bare NPE from an explicit `throw new NullPointerException()` vs
        // Objects.requireNonNull vs a helpful-NPE dereference, for calibration
        cls = "calibration";
        row("new NPE().getMessage()", () -> { throw new NullPointerException(); });
        row("Objects.requireNonNull(null)", () -> Objects.requireNonNull(null));
        row("((Object)null).hashCode()", () -> { Object o = null; return o.hashCode(); });
    }
}
```

### 7.2 `scratchpad/g1/Probe2.java` — argument-check ORDER, and present-vs-absent keys

Written because `Probe.java` alone cannot tell "the value is checked" from
"the value is checked *first*", and because its value-null cells all used an
**absent** key — which would have hidden `Hashtable.putIfAbsent`'s
unconditional `requireNonNull` and `Hashtable.merge`'s missing one.

```java
import java.util.*;
import java.util.concurrent.*;
import java.util.function.*;

public class Probe2 {
    interface Cell { Object run() throws Throwable; }
    static String cls = "?";
    static void row(String label, Cell c) {
        String out;
        try { Object r = c.run(); out = "OK -> " + (r == null ? "null" : ("\"" + r + "\"")); }
        catch (Throwable t) {
            String m = t.getMessage();
            out = t.getClass().getName() + " msg=" + (m == null ? "<NO-MESSAGE>" : "<<" + m + ">>");
        }
        System.out.println(pad(cls,24) + "| " + pad(label,36) + "| " + out);
    }
    static String pad(String s,int n){StringBuilder b=new StringBuilder(s);while(b.length()<n)b.append(' ');return b.toString();}
    static final BiFunction<Object,Object,Object> BF = (a,b) -> "bicomputed";
    static final Function<Object,Object> F1 = k -> "computed";

    static void order(String name, Supplier<Map<Object,Object>> mk) {
        cls = name;
        // ORDERING: which argument is checked first when several are null
        row("put(null,null)",                () -> mk.get().put(null,null));
        row("remove(null,null)",             () -> mk.get().remove(null,null));
        row("replace(null,null)",            () -> mk.get().replace(null,null));
        row("replace(null,null,null)",       () -> mk.get().replace(null,null,null));
        row("replace(k,null,null)",          () -> mk.get().replace("k",null,null));
        row("putIfAbsent(null,null)",        () -> mk.get().putIfAbsent(null,null));
        row("merge(null,null,null)",         () -> mk.get().merge(null,null,null));
        row("merge(null,v,null)",            () -> mk.get().merge(null,"v",null));
        row("merge(null,null,f)",            () -> mk.get().merge(null,null,BF));
        row("compute(null,null)",            () -> mk.get().compute(null,null));
        row("computeIfAbsent(null,null)",    () -> mk.get().computeIfAbsent(null,null));
        row("computeIfPresent(null,null)",   () -> mk.get().computeIfPresent(null,null));
        // PRESENT-KEY variants of the value-null cells (probe 1 used an ABSENT key)
        row("putIfAbsent(PRESENT k,null)",   () -> mk.get().putIfAbsent("k", null));
        row("computeIfAbsent(PRESENT k,null)", () -> mk.get().computeIfAbsent("k", null));
        row("merge(PRESENT k,null,f)",       () -> mk.get().merge("k", null, BF));
        row("merge(ABSENT k,null,f)",        () -> mk.get().merge("zz", null, BF));
        row("computeIfPresent(ABSENT k,null)", () -> mk.get().computeIfPresent("zz", null));
        row("compute(ABSENT k,null)",        () -> mk.get().compute("zz", null));
        row("remove(ABSENT k,null)",         () -> mk.get().remove("zz", null));
        row("replace(ABSENT k,null)",        () -> mk.get().replace("zz", null));
        row("replace(ABSENT k,null,n)",      () -> mk.get().replace("zz", null, "n"));
        row("containsValue on empty(null)",  () -> { Map<Object,Object> m = mk.get(); m.clear();
                                                     return m.containsValue(null); });
        System.out.println();
    }

    public static void main(String[] a) {
        System.out.println("ORACLE " + System.getProperty("java.runtime.version"));
        order("java.util.HashMap",  () -> { Map<Object,Object> m=new HashMap<>(); m.put("k","v"); return m; });
        order("java.util.Hashtable",() -> { Map<Object,Object> m=new Hashtable<>(); m.put("k","v"); return m; });
        order("java.util.Properties",()-> { Properties p=new Properties(); p.put("k","v"); return p; });
        order("j.u.c.ConcurrentHashMap",()->{ Map<Object,Object> m=new ConcurrentHashMap<>(); m.put("k","v"); return m; });
    }
}
```

### 7.3 Oracle output — `Probe2.java`, the ordering block that settles §2a

```text
ORACLE 25.0.3+9-LTS
java.util.HashMap       | put(null,null)                      | OK -> null
java.util.HashMap       | remove(null,null)                   | OK -> "false"
java.util.HashMap       | replace(null,null)                  | OK -> null
java.util.HashMap       | replace(null,null,null)             | OK -> "false"
java.util.HashMap       | replace(k,null,null)                | OK -> "false"
java.util.HashMap       | putIfAbsent(null,null)              | OK -> null
java.util.HashMap       | merge(null,null,null)               | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | merge(null,v,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | merge(null,null,f)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | compute(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | computeIfAbsent(null,null)          | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | computeIfPresent(null,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | putIfAbsent(PRESENT k,null)         | OK -> "v"
java.util.HashMap       | computeIfAbsent(PRESENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | merge(PRESENT k,null,f)             | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | merge(ABSENT k,null,f)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | computeIfPresent(ABSENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | compute(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.HashMap       | remove(ABSENT k,null)               | OK -> "false"
java.util.HashMap       | replace(ABSENT k,null)              | OK -> null
java.util.HashMap       | replace(ABSENT k,null,n)            | OK -> "false"
java.util.HashMap       | containsValue on empty(null)        | OK -> "false"

java.util.Hashtable     | put(null,null)                      | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | remove(null,null)                   | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | replace(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | replace(null,null,null)             | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | replace(k,null,null)                | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | putIfAbsent(null,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | merge(null,null,null)               | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | merge(null,v,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | merge(null,null,f)                  | java.lang.NullPointerException msg=<<Cannot invoke "Object.hashCode()" because "key" is null>>
java.util.Hashtable     | compute(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | computeIfAbsent(null,null)          | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | computeIfPresent(null,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | putIfAbsent(PRESENT k,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | computeIfAbsent(PRESENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | merge(PRESENT k,null,f)             | OK -> "bicomputed"
java.util.Hashtable     | merge(ABSENT k,null,f)              | OK -> null
java.util.Hashtable     | computeIfPresent(ABSENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | compute(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | remove(ABSENT k,null)               | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | replace(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | replace(ABSENT k,null,n)            | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Hashtable     | containsValue on empty(null)        | java.lang.NullPointerException msg=<NO-MESSAGE>

java.util.Properties    | put(null,null)                      | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | remove(null,null)                   | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | replace(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | replace(null,null,null)             | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | replace(k,null,null)                | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | putIfAbsent(null,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | merge(null,null,null)               | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | merge(null,v,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | merge(null,null,f)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | compute(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | computeIfAbsent(null,null)          | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | computeIfPresent(null,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | putIfAbsent(PRESENT k,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | computeIfAbsent(PRESENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | merge(PRESENT k,null,f)             | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | merge(ABSENT k,null,f)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | computeIfPresent(ABSENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | compute(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | remove(ABSENT k,null)               | OK -> "false"
java.util.Properties    | replace(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | replace(ABSENT k,null,n)            | java.lang.NullPointerException msg=<NO-MESSAGE>
java.util.Properties    | containsValue on empty(null)        | java.lang.NullPointerException msg=<NO-MESSAGE>

j.u.c.ConcurrentHashMap | put(null,null)                      | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | remove(null,null)                   | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | replace(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | replace(null,null,null)             | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | replace(k,null,null)                | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | putIfAbsent(null,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | merge(null,null,null)               | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | merge(null,v,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | merge(null,null,f)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | compute(null,null)                  | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | computeIfAbsent(null,null)          | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | computeIfPresent(null,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | putIfAbsent(PRESENT k,null)         | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | computeIfAbsent(PRESENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | merge(PRESENT k,null,f)             | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | merge(ABSENT k,null,f)              | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | computeIfPresent(ABSENT k,null)     | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | compute(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | remove(ABSENT k,null)               | OK -> "false"
j.u.c.ConcurrentHashMap | replace(ABSENT k,null)              | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | replace(ABSENT k,null,n)            | java.lang.NullPointerException msg=<NO-MESSAGE>
j.u.c.ConcurrentHashMap | containsValue on empty(null)        | java.lang.NullPointerException msg=<NO-MESSAGE>
```

The full `Probe.java` output is `scratchpad/g1/oracle-out.txt`; §2 is its
transposition and nothing was dropped.

---

## 8. What this says about the method

HANDOFF §5's "do not generalise a contract from three rows" held twice more
here, and in both directions.

Generalising **up** from `Properties` would have given `Hashtable` a
message-less refusal everywhere, which is right for eight cells and wrong for
seven — and *invisible* to any assertion that checks the exception type, which
is how the two rows F41-1 scored as matching got there. Generalising **down**
from `HashMap.merge` would have made `Hashtable.merge(k, null, f)` throw where
the JDK computes.

The `ConcurrentHashMap` half is the cheaper lesson: ten strings, ten
functions, zero of them ever compared with anything. They were all *plausible*
— they name the class, they describe the rule, they read like a JDK message.
Plausibility is the failure mode. `javap -p -c` answered all nine in one
command, and the answer was that nine of them should be nothing at all.
