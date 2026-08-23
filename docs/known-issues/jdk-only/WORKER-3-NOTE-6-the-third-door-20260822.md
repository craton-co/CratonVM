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

Verified twice: on the base this work was cut from (`8104bfd2f`) and again on
the MERGED state (`e15560b32`), because the base branch moved 23 files under it
mid-task. The second pass is the one that matters — it found a failure the first
could not have seen.

**On `8104bfd2f`:** `--jdk-only` 107/107; `SUITE=all` 106/107; `SUITE=core`
66/67 across 3 runs, the only failure being `RTreeRangeGc`.

**On the merged `e15560b32`:** the probe still answers `8 8`, and

| arm | mine | CONTROL — their tip, this fix reverted |
|---|---|---|
| `--jdk-only` run 1 | 106/107 `RJdkJmx` | 105/107 `RTreeRangeGc` `RJdkJmx` |
| `--jdk-only` run 2 | 105/107 `RTreeRangeGc` `RJdkJmx` | 106/107 `RJdkJmx` |
| `SUITE=all` | 106/107 `RTreeRangeGc` | — |
| `SUITE=core` | 66/67 `RTreeRangeGc` | — |

**Identical failure populations.** Neither failure is this change.

* `RTreeRangeGc` — the open GC issue in
  `bug-zgc-relocation-unmasks-root-collection-gap-rtreerangegc-20260821.md`;
  matching `cratonvm::gc::guard` signature, and a **pristine rebuild of
  `8104bfd2f` fails it identically** (2 runs). That record calls it intermittent
  under the corpus; today it was reliably red in `SUITE=core` (5 runs across two
  binaries) and intermittent under `--jdk-only`, which is worth a line there.
* **`RJdkJmx` is a REGRESSION ON THE BASE BRANCH, not here.** It fails **2 of 2**
  on `9a7104199` with this fix reverted, and does not appear in this lane's
  pre-merge 107/107. Symptom:
  `CK RJdkJmx objectName=cratonvm.test:name=alpha,type=Counter` — a cross-VM
  diff on an `ObjectName` rendering. **Flagged for H0; it is not a `java.lang`
  row and this lane is not taking it.**

Census, `--jdk-only`, counted by TRIPLE: **1443 native-won**, unchanged by this
fix as predicted — a `kind=intrinsic` row cannot be counted at all (`NOTE-5`).

### A note on the control that nearly did not happen

The first control build died `rc=143` (SIGTERM — this host runs ~470 sessions
and broad `pkill`s land on other people's cargo). Because the build script only
copies its binary on `rc=0`, the previous binary was still in place, and a
`cp` of it produced a "control" that was **the fixed binary under another
name**. Caught by checking the build log's `rc` and the binary's mtime rather
than its existence. Any A/B on this host should assert both before believing a
result.

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

* `WORKER-3-NOTE-6` §5 — `RJdkJmx` is a REGRESSION ON THE BASE BRANCH: it fails
  2 of 2 on `9a7104199` with this lane's fix reverted, on an `ObjectName`
  rendering diff. H0 to route it; it is not a `java.lang` row.
