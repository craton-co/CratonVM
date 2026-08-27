# `CRATONVM_BG_COMPILE=0` nulls a reference argument — FIXED, and it was never about the lever

**Status: FIXED 2026-08-27.** `TechEmpowerTest` passes **4/4** with
`CRATONVM_BG_COMPILE=0` and **3/3** on the default configuration, against a
recorded baseline of **5/5 FAIL in 9 s** across two binaries. Regression suite
72/72.

Three separate defects, found in this order because each was hiding the next.
Only the first is what the original page was about; the other two are what the
9-second death had been standing in front of.

## 1. An `invokevirtual` that resolves to a PRIVATE method is not a dispatch site

**This is the defect, and it is a DEFAULT-configuration defect.** The original
page's third instruction — "do not assume it is specific to `BG_COMPILE=0`" —
was right, and the reason is stronger than the page guessed: the lever changed
nothing about the bug except when it fires.

JVMS §5.4.6 selects a private method as the one the constant pool resolved, with
no override lookup. javac has emitted `invokevirtual` for a call to a private
instance method since Java 11 (JEP 181 nestmates), where it used to emit
`invokespecial` — the opcode changed, the semantics did not. Every compiled
dispatcher downstream of `invoke_kind == 0` resolves by walking up from the
**receiver's** class, so on such a site it finds the most-derived same-named
private method and calls that one.

The interpreter has never had this bug: `resolved_private_invokevirtual_target`
pins it. Each compile door classified invoke sites for itself, and none of them
asked.

### Why Vert.x

```java
// io/vertx/core/net/TCPSSLOptions
public TCPSSLOptions() { super(); init(); }         // invokevirtual, PRIVATE init
private void init() { …; transportOptions = new TcpConfig(); … }
```

`ClientOptionsBase` and `HttpClientOptions` each declare their own
`private void init()`. Constructing a `WebClientOptions` therefore ran
`HttpClientOptions.init()` **three times** and `TCPSSLOptions.init()` **never**.
`transportOptions` was left null, `getTransportOptions()` returned it, and

```
TcpEndpointConfig.<init>(TCPSSLOptions):
   9: aload_1
  10: invokevirtual TCPSSLOptions.getTransportOptions()   ← returns null
  13: invokespecial TcpConfig."<init>":(TcpConfig)        ← `other` is null
```

which is the reported NPE, nine seconds in. The page's second instruction —
"bisect the argument, not the callee" — pointed at the caller; the argument was
a **returned** null, and disassembling one level further back is what found it.

### The probe

`apps/jit-probes/PrivateVirtualInitProbe.java` — three levels, each with a
constructor calling its own `private void init()`, 200 000 iterations. On the
binary that failed:

| arm | baseField | middleField | leafField | first bad |
|---|---:|---:|---:|---:|
| HotSpot | 200000/200000 | 200000/200000 | 200000/200000 | — |
| CratonVM, **default** | **2000**/200000 | **2000**/200000 | 200000/200000 | 2000 |
| CratonVM, `BG_COMPILE=0` | **1000**/200000 | **1000**/200000 | 200000/200000 | 1000 |
| CratonVM, `--nojit` | 200000/200000 | 200000/200000 | 200000/200000 | — |

The first correct iterations are the interpreted ones; the wrongness starts at
the compile threshold, and `leafField` is right throughout because the
most-derived `init()` is the one dispatch keeps landing on. All arms pass after
the fix.

### The fix

Every door that classifies an invoke site now asks
`invoke::invokevirtual_site_targets_private` and, on `true`, binds the site
directly at the declaring class (`invoke_kind == 1`) instead of dispatching:

* `jit::try_compile_inner` — single-pass invoke loop **and** the IR tier's
  `is_virtual`/`is_special` derivation, through the resolver that already
  answers the JVMS §6.5 `invokespecial` question (it now takes the opcode);
* `interpreter.rs`'s eager first-call door and `jit_bridge.rs`'s OSR door, both
  of which reach `x64::compile_with_param_slots` directly and inherit nothing
  from `try_compile`.

**Fixing only `try_compile` fixed only the default configuration.** The
`BG_COMPILE=0` arm stayed at 3/3 FAIL with the identical NPE until the eager
first-call door was fixed too — that door's own comment says it is "only
reachable with `CRATONVM_BG_COMPILE=0`". A lever that selects a different
compile door selects a different set of unfixed bugs.

`PRIVATE_INVOKEVIRTUAL_PINNED` is the engagement counter: **1768** sites on one
TechEmpower run, printed by the end-of-run report. It exists because "the probe
passes now" and "no site on this workload was one" are not the same fact.

## 2. The callee-side `ldc` resolver baked the CALLER's class id

With the NPE gone the arm ran the workload and failed 3/3 on

```
InternalError: JIT ldc: cp#47 of class id 5204 is not a readable string literal
    at org.hibernate.reactive.pool.impl.SqlClientConnection.<init>
```

`c_ldc_resolver` reads the **callee's** constant pool (`cm.get_class(callee_cid)`
three lines above), so its `cp_idx` indexes that pool. Its `String` arm paired it
with `class_id` — the **caller**. The `ClassReference` arm immediately below
always used `callee_cid`.

Compiling `SqlClientPool.newConnection`, which inlines
`SqlClientConnection.<init>`, baked `(SqlClientPool, cp#47)` for a literal that
lives at cp#47 of `SqlClientConnection`:

| class | cp#47 |
|---|---|
| `SqlClientConnection` | `String "Connection created for %1$s associated to context %2$s:"` |
| `SqlClientPool` | `Class io/vertx/sqlclient/Pool` |

**The `InternalError` is the benign half.** Had the caller held a `String` at
that index instead, compiled code would have pushed the **wrong literal** and
reported nothing.

Finding it took one diagnostic change: the helper's single error message covered
three different failures, and named only the one that cannot happen ("the
constant pool says otherwise" — the compile-time resolver already matched the
entry). Split into "no class at that id" / "that class holds *X* there" /
"unreadable utf8", the first run printed the class name and the entry kind.

## 3. Three silent wrong-slot substitutions, closed on the way past

None of these turned out to be the cause here — the counters read **0** on every
arm — but all three are the same hazard class and all three were live:

* **`ir_lower.rs`**: `Op::Load`/`Op::Store`'s field index came from
  `match … { Op::Const(v) => v, _ => 0 }`. Slot 0 is a *different field of the
  same receiver*. Now refused in `lower_inner`, with an unconditional deopt as
  the belt-and-braces arm.
* **`bytecode_walk.rs`**: four `.unwrap_or((pc, 0, b'I'))` field defaults —
  **slot 0, tagged int**. This has been caught corrupting a heap cell before
  (JDT `HashtableOfInt.rehash` storing an `int[]` as `Value::Int(low32_of_ptr)`);
  the fix then populated one table and left the default. Now a counted bail,
  with `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE=1` to restore it for a
  single-binary A/B.
* **`jit_bridge.rs`**: field resolution matches by **name alone**
  (`find_own_field`, `find_field_recursive`, `locate_field`), while the JIT takes
  the type tag from the constant-pool descriptor. A site where the two disagree
  is now refused and counted.

## What the counters said, and why that mattered

Every mechanism this investigation could have blamed was ruled out by a number
before any of them cost a reproduction attempt:

```
unresolved field sites refused: 0
field sites refused for a descriptor disagreement: 0
compiled field stores dropped: implausible receiver=0  slot out of bounds=0
getfield reference loads that contained a primitive slot: 2377701
    (of which the payload word was NON-ZERO: 0)
```

That last line is the warning in the set. 2.4 million punned reference reads on
a **passing** run — the benign never-assigned-cell population — which is why the
raw count is not evidence of anything and why the report now filters on the
class rather than on the count.

## Related

* `punned-sqlchar-rawdata-cell-writer-localized-…` — the sibling page whose
  instrument this work un-blinded (`jit_putfield_*` bypasses the collector's
  store watch entirely).
* `techempower-wrong-answer-was-the-indy-trap-FIXED-20260824.md` — the
  workload's other, unrelated defect.
