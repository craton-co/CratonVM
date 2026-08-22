# WORKER-3-NOTE-6 — the third door was a `java/lang/String` native, and it was fabricating

**2026-08-22**, on `8104bfd2f`. `WORKER-3-NOTE-4` §4.5 left a door unidentified:
`sb_state` was still entered four times with the builder registrations skipped
**and** `CRATONVM_DISABLE_INTRINSICS=1`. This closes it.

## 1. It is not a third dispatch mechanism

A backtrace at `sb_state` names the caller in one line:

```text
[w3bt] sb_state count=8 buf_is_char=false
   0: sb_state                               at native-builtins/src/lang_string.rs:1549
   1: native_string_init_from_string_builder at native-builtins/src/deprecated_util.rs:662
   …
  10: try_stackless_invoke                   at vm/src/runtime/interpreter/invoke.rs:3945
```

`native_string_init_from_string_builder` is an ordinary registry `Bridge`-shaped
native — registered on **`java/lang/String`**, a class no builder retirement
names. That is the whole trick: retiring `StringBuilder`,
`AbstractStringBuilder` and `StringBuffer` does not touch the natives their
*callees* reach.

It survives `CRATONVM_DISABLE_INTRINSICS` too, because that flag governs the
**interpreter intrinsic table** (`intrinsics::lookup`), a different mechanism
from the registry's `NativeKind::Intrinsic`, which is what this registration
carries. Two unrelated things named "intrinsic", one switch, and it is not the
one you would reach for.

It is also, being `kind=intrinsic`, one of `WORKER-3-NOTE-5`'s **398
census-exempt shadows** — so it could never have appeared in the count either.

## 2. The wrong overload is answering the call

MEASURED, and this is the sharper half. JDK 25's `StringBuilder.toString()`
compiles to `new String(this, null)`, i.e.
`String.<init>(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V`. Both
overloads are registered. After a program that constructs **exactly one** String
from a builder (`SbProbe6`, which contains no string concatenation at all, so
`StringConcatFactory` cannot be building through a hidden builder):

| registered descriptor | kind | owns_slot | **invocations** |
|---|---|---|---:|
| `(Ljava/lang/AbstractStringBuilder;Ljava/lang/Void;)V` | intrinsic | true | **0** |
| `(Ljava/lang/StringBuilder;)V` | intrinsic | true | **1** |

The call whose descriptor is `(ASB,Void)V` was serviced by the `(StringBuilder)V`
body. The native written *for* that call site has never run.

**Not fixed here** — the dispatch lives in
`vm/src/runtime/interpreter/`, which is WORKER 1's file set, and it is a
descriptor-selection question rather than a `java.lang` one. Filed as a
follow-up. It is the same shape as the recorded *two overloads, one callback*
family, and worth checking for more instances.

## 3. What it was doing — a fabrication, and the third copy of one read

```rust
let (buf, count) = sb_state(ctx, sb);
let units = match buf {
    Some(b) => { /* read the char[] */ }
    None    => Vec::new(),        // <- here
};
```

`sb_state` yields `None` for a builder whose `value` is the real compact
`byte[]` — which is exactly what real `AbstractStringBuilder` bytecode produces.
`Vec::new()` for that receiver is a **fabrication, not a refusal**: the caller
cannot tell an empty builder from one this constructor declined to read.

Three copies of "read the builder's chars" exist in the tree — `sb_read_chars`,
`native_sb_to_string`, and this one. The first two were given the layout-aware
`sb_value_units` fallback. **This one was missed, and it is the copy on the live
path.**

## 4. The fix, and what it does NOT buy

One arm, matching its two siblings: read either layout, truncate to `count`.

MEASURED, `SbProbe6` (`b.append("abcdefgh")`, then `b.toString().length()`):

| config | before | after |
|---|---:|---:|
| registrations + intrinsic table both closed | **0** | **8** |
| registrations closed | 0 then 8 | 8, 8 |
| control — nothing retired | 8 | 8 |

The default path is untouched by construction: a `char[]`-backed builder takes
the `Some(b)` arm, which is unchanged.

**It does not unblock the builder retirement.** Re-priced after the fix,
`W3_RETIRE=<the three builder classes>` is still **95/107 with the same twelve
vectors**, `RStringBuilderContent` among them. The fabrication and the
retirement blocker are different problems, and `WORKER-3-NOTE-4` §4.3's refusal
stands unchanged.

## 5. Verification

| arm | result |
|---|---|
| `CRATONVM_ARGS=--jdk-only` | **107 / 107** |
| `SUITE=all` | 106 / 107 — `RTreeRangeGc` |
| `SUITE=core` | 66 / 67 — `RTreeRangeGc`, 3 runs |
| **pristine `8104bfd2f`, `SUITE=core`** | **66 / 67 — `RTreeRangeGc`, 2 runs** |

`RTreeRangeGc` is **not** this change. It is the open GC issue recorded in
`bug-zgc-relocation-unmasks-root-collection-gap-rtreerangegc-20260821.md`, the
failure text matches that record's signature (`cratonvm::gc::guard`), and a
pristine rebuild of the same commit fails it identically. That doc calls it
intermittent under the corpus; on this host today it is reliably red in
`SUITE=core` and green under `--jdk-only`, which is worth a line in that record.

Census, `--jdk-only`, counted by TRIPLE: **1443 native-won**. Unchanged by this
fix, as predicted — a `kind=intrinsic` row cannot be counted (`NOTE-5`).

## Index rows for `INDEX.md` (H0 to place)

* `WORKER-3-NOTE-6` — the third door was an ordinary registry native on
  `java/lang/String`: a class-scoped retirement does not close the natives its
  CALLEES reach, and `CRATONVM_DISABLE_INTRINSICS` governs the interpreter
  intrinsic table, not `NativeKind::Intrinsic`
* `WORKER-3-NOTE-6` §2 — `String.<init>(AbstractStringBuilder,Void)V` has **0
  invocations** while `(StringBuilder)V` has 1 for a call whose descriptor is
  the former: the wrong overload answers, and the native written for the call
  site has never run
* `WORKER-3-NOTE-6` §3 — FIXED: the third copy of the builder read was
  fabricating `""` for a `byte[]`-backed builder; the two siblings were fixed
  and this one, the live one, was missed
