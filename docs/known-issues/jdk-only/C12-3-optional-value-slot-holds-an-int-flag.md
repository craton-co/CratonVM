# C12-3 — `java.util.Optional`'s `value` slot holds an `int` flag, and `isPresent()` reads the flag as the value

**2026-08-12, lane C12.** Raised as an unmeasured flag in
`C6-1-https-urlconnection-session-accessors.md` NOMINATION 2: *"`java.util.Optional`
appears to have TWO INCOMPATIBLE synthetic layouts"*. **Verified, and it is
larger and simpler than the flag said.** The line numbers are right; the framing
("two layouts") is not the defect. The defect is one idiom applied to a real JDK
class whose single field is a REFERENCE:

> **every** `Optional` these natives build stores an `int` presence flag in slot
> 0 — and on a real `java.util.Optional`, slot 0 **is** `value`, the field
> `isPresent()`, `get()`, `orElse()`, `ifPresent()` and `map()` all read.

The arity disagreement (1 slot vs 2) is a symptom: the 2-slot sites park the
real value at slot 1, which no real bytecode ever reads.

**This lane could not build or run the VM.** The JDK bytecode below was
disassembled on this host (HotSpot 25.0.3+9-LTS); the CratonVM behaviour is
**PREDICTED** from that bytecode plus the interpreter's own null test, and the
run that settles it is named at the end. **This lane owns neither file, so
everything here is a NOMINATION.**

---

## 1. The real class has exactly one instance field, and it is a reference

```
$ javap -p --module java.base java.util.Optional
  private static final java.util.Optional<?> EMPTY;
  private final T value;
```

```
$ javap -p -c --module java.base java.util.Optional
  public boolean isPresent();
       0: aload_0
       1: getfield  #13   // Field value:Ljava/lang/Object;
       4: ifnull    11
       7: iconst_1
       8: goto      12
      11: iconst_0
      12: ireturn

  public T get();
       1: getfield  #13   // Field value:Ljava/lang/Object;
       4: ifnonnull 17
       7: new       #26   // class java/util/NoSuchElementException
      ...
      18: getfield  #13
      21: areturn
```

One field. `isPresent()` is `value != null`; `get()` returns `value` itself.
There is no flag, and there is nowhere to put one.

## 2. What the natives write

| site | slots asked | slot 0 | slot 1 |
|---|---|---|---|
| `classfile_api.rs:39` (`empty_optional`) | 1 | `Value::Object(None)` | — |
| `classfile_api.rs:234, 304, 383, 387` | 1 | a real reference | — |
| `jdk25_concurrency.rs:1385` | `OPTIONAL_NUM_FIELDS` = **1** | a real reference | — |
| `http2.rs:1289` `connectTimeout()` | **2** | `Int(ms > 0)` | `Long(ms)` |
| `http2.rs:1706` `bodyPublisher()` | **2** | `Int(has_body)` | the publisher ref |
| `http2.rs:1727` `timeout()` | **2** | `Int(ms > 0)` | `Long(ms)` |
| `http2.rs:1750` `version()` | **2** | `Int(ver != 0)` | `Int(ver - 1)` |
| `http2.rs:2121` `sslSession()` | **2** | `Int(has)` | the session ref |
| `http2.rs:2209` | **2** | `Int(...)` | — |
| **`http2.rs:2108` `previousResponse()`** | **1** | **`Int(has)`** | — |

**`previousResponse()` is the row that settles what this is.** It uses the
correct arity and still writes an `Int` into slot 0 — so "two layouts" is not
the disease. The disease is that these natives model an `Optional` as
*(flag, payload)* and the real class models it as *(reference-or-null)*.

`classfile_api.rs` and `jdk25_concurrency.rs` are **correct** and are the
in-tree precedent: `jdk25_concurrency.rs:84` even declares
`const OPTIONAL_NUM_FIELDS: usize = 1;` with an assertion pinning it
(`:3482`). The right answer already exists in this workspace with a test on it.

## 3. Why it is not a benign over-allocation

**It does not crash, and that is the problem.** `try_alloc_concurrent_synthetic`
allocates `num_fields.max(real)` slots (`util_concurrent_ext.rs`, the `let n =`
line), so a 2-slot request over a 1-field class yields a 2-slot object and the
GC's bounds guard is satisfied. Slot 1 is simply invisible to bytecode. What
reaches Java is a well-formed `java.util.Optional` whose `value` is an `int`.

Now run the real bytecode against it. The interpreter's shared null test is
explicit about what counts as null (`vm/src/runtime/interpreter.rs:8232`,
`ref_operand_is_null`): `Value::Object(None)`, `Value::Uninitialized` and
`Value::Long(0)` — and **nothing else**. `Value::Int(0)` is not null.

**PREDICTED**, on any of the eight `http2.rs` sites:

| call | HotSpot | CratonVM (PREDICTED) |
|---|---|---|
| `req.timeout().isPresent()` with no timeout set | `false` | **`true`** — `ifnull` on `Int(0)` does not take the branch |
| `req.timeout().get()` with no timeout set | `NoSuchElementException` | an `Int(0)` returned where a `Duration` is declared |
| `req.timeout().get()` with a timeout set | a `Duration` | `Int(1)` — **the flag**, not the `Long` at slot 1 |
| `resp.sslSession().isPresent()` with no session | `false` | **`true`** |
| `resp.sslSession().get()` with a session | the `SSLSession` | `Int(1)` |

So the presence flag inverts the empty case AND becomes the returned value in
the present case. **`orElse(x)` returns the flag; `ifPresent(c)` invokes the
consumer with the flag; `map(f)` applies `f` to the flag.** The `Long`/`Duration`
the caller wanted is in slot 1, which nothing reads.

This is this project's recorded shape *"a defaulting reader turns a wrong type
into a quiet wrong write"*, and it is on a **real JDK class**, which is the
category *"OOB field read on a real JDK class is a real bug"* was written for.
Nothing here is an out-of-bounds ACCESS — the object is over-allocated, not
under-allocated — so it is a wrong-type read, not a memory-safety fault. The
distinction matters and this record does not claim the stronger one.

**Why it has survived:** every affected accessor is on `java.net.http.HttpRequest`
/ `HttpResponse` config getters, which applications set and rarely read back.
`sslSession()` is the one an application plausibly calls.

## 4. The instrument that already sees it

`try_alloc_concurrent_synthetic` calls
`cratonvm_native_api::layout_alias::classify(num_fields, real)` on every
allocation and reports when they disagree. `classify(2, 1)` is `Some`, so
**each of the eight 2-slot sites already emits a layout-alias observation naming
`java/util/Optional`**, attributed by `#[track_caller]` to the native that asked.
The evidence is presumably already in any layout-alias census taken since that
reporting landed — nobody has read the rows.

`previousResponse()` (1 slot, right arity, wrong type) is **invisible** to that
instrument, because the instrument compares counts and this row's count is
correct. A wrong-type write has no detector here.

## NOMINATION — `native-builtins/src/http2.rs` (not this lane's file)

**The shape of the fix, once, for all nine sites:** an `Optional` is `value` or
null. Never a flag.

REPLACE (`http2.rs:1289`, `connectTimeout()`):

```rust
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 2)?;
            ctx.set_field(opt, 0, Value::Int(if ms > 0 { 1 } else { 0 }));
            ctx.set_field(opt, 1, Value::Long(ms));
            Ok(Some(Value::Object(Some(opt))))
```

WITH:

```rust
            // `java.util.Optional` has ONE instance field and it is a
            // reference: `isPresent()` is `value != null` and `get()` returns
            // `value` itself (javap, JDK 25). A presence flag in slot 0 IS the
            // value as far as that bytecode is concerned, and `Value::Int(0)`
            // is not null to `ref_operand_is_null` — so a flag of 0 reads as
            // PRESENT and a flag of 1 is what `get()` hands back. See
            // docs/known-issues/jdk-only/C12-3-optional-value-slot-holds-an-int-flag.md
            let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
            if ms > 0 {
                let d = alloc_duration_millis(ctx, ms)?;
                ctx.set_field(opt, 0, Value::Object(Some(d)));
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
```

and the same transformation at `:1706` (the publisher ref straight into slot 0,
`Object(None)` when absent), `:1727`, `:1750`, `:2108`, `:2121`, `:2209`.

**`alloc_duration_millis` does not exist and is the real work in this
nomination.** `connectTimeout()` and `timeout()` are declared
`Optional<Duration>`, so the honest fix has to produce a `java.time.Duration`,
not a `Long` — the current code does not, at either slot. Two of the sites
(`version()`, which owes an `HttpClient$Version`) have the same problem and the
file already has an enum-mirror helper for it (`version_enum`, used by the
non-`Optional` `version()` a few lines above, whose own comment records this
exact class of bug: *"Declared to return `HttpClient$Version`; returning the raw
slot int handed an Int where every caller dereferences an object"*). **That
comment is the same defect, already found once in this file, and fixed only for
the non-`Optional` twin.**

An acceptable smaller first step, if a `Duration` mirror is too much: write
`Object(None)` in slot 0 unconditionally at the timeout sites — i.e. always
answer `Optional.empty()`. That is a **missing answer** where today there is a
wrong one, it matches the common real configuration (no timeout set), and it
cannot make `isPresent()` lie.

## How to settle it

Nothing here needs a TLS peer or a network. One class:

```java
import java.net.http.HttpClient; import java.time.Duration;
public class OptShape {
  public static void main(String[] a) {
    HttpClient c = HttpClient.newBuilder().build();           // no timeout set
    System.out.println("isPresent=" + c.connectTimeout().isPresent());
    System.out.println("class=" + c.connectTimeout().getClass().getName());
    try { System.out.println("get=" + c.connectTimeout().get()); }
    catch (Throwable t) { System.out.println("get THREW " + t); }
  }
}
```

HotSpot 25 prints `isPresent=false` / `class=java.util.Optional` and
`NoSuchElementException` from `get()`. **PREDICTED on CratonVM today:
`isPresent=true`, and `get()` returning something that is not a `Duration`.** If
CratonVM instead prints `isPresent=false`, then `ifnull` treats `Int(0)` as null
somewhere on this path and §3's first row is wrong — in which case the remaining
rows (a flag of 1 being returned by `get()`) still stand and are the ones to
check.

Run it with the layout-alias census enabled as well: `java/util/Optional` should
appear with `declared=1` against `requested=2`, from eight distinct
`requested_by` natives.

## Residuals

1. **This record does not claim a memory-safety fault.** The objects are
   over-allocated, so every access is in bounds. It is a wrong-type read.
2. **`http2.rs:2209` was read in less detail than the others** — the same 2-slot
   allocation, but this lane did not trace what the payload is meant to be.
3. **`http_client.rs` uses the 1-slot form at eight sites** (`:1500`, `:1570`,
   `:1576`, `:1604`, `:1610`, `:1620`, `:1632`, `:1652`) and was not audited for
   whether any of them writes a non-reference into slot 0 the way
   `previousResponse()` does. Right arity is not evidence of the right type —
   that is this record's whole point, and it applies to the sites it did not
   read.
