# H0-1 — the JMX ambient pin, an `ObjectName` slot index converted, and a JDK path that was not there

**Status: FIXED-UNVERIFIED.** Three source/doc changes landed in the tree. **No
binary carrying them has been built or run.** The `--jdk-only` arm referenced in
§6 is the **baseline at `26e4b5db4` without these changes**, measured on this
host today; it is the control this lane's changes must stay verdict-neutral
against, not evidence about them.

Lane H0 (orchestrator), 2026-08-20. Branch `claude/jdk-only-mode-handoff-09b48c`.

---

> **VERIFIED AGAINST A BINARY 2026-09-04.** Status was **FIXED-UNVERIFIED** —
> *"No binary carrying them has been built or run"* — with §6 explicitly a
> control at `26e4b5db4` that these changes *"must stay verdict-neutral
> against, not evidence about them"*.
>
> **§3, the `ObjectName` conversion, is byte-identical to the oracle in both
> modes.** `probes/ONProbe.java` reads the accessors whose answers a
> layout-indexed-by-number would confuse:
>
> ```text
>                             HotSpot 25 / compatible / --jdk-only  (all identical)
> A getCanonicalName          java.lang:name=G1 Young Generation,type=GarbageCollector
> A getKeyPropertyListString  type=GarbageCollector,name=G1 Young Generation
> A toString                  java.lang:type=GarbageCollector,name=G1 Young Generation
> A getCanonicalKeyPropList   name=G1 Young Generation,type=GarbageCollector
> B getCanonicalName          d:a=1,b=2,c=3
> B getKeyPropertyListString  b=2,a=1,c=3
> B toString                  d:b=2,a=1,c=3
> ```
>
> **The three orderings disagreeing with each other is the result**, not the
> individual values. `getCanonicalName` sorts the keys, `getKeyPropertyListString`
> preserves insertion order, and `toString` preserves the original spelling —
> case B shows all three diverging on the same input. A conversion that indexed a
> real layout by number would collapse at least two of them onto one answer, and
> a probe reading only `toString` would not notice.
>
> **§6's verdict-neutrality holds.** The changes' own vectors, and the arm the
> control measures:
>
> ```text
> RJdkJmx                  PASS (67 checks)    compatible and --jdk-only
> RImmutableFactoryTypes   PASS (219 checks)   compatible and --jdk-only
> full suite               --jdk-only 129 passed / 0 failed
> ```
>
> **What this does NOT verify.** §2's *"the two registrars are pinned"* is a
> source claim about registration order; the dump was not taken for it here and
> no probe distinguishes which registrar answered. §1 — the oracle JDK not being
> where the handoff says — is a correction about this host's paths and needs no
> VM. §§7-8, the out-of-file edits and nominations, are untouched. §6's control
> was measured at `26e4b5db4`; the run above is on today's tree, so it shows the
> arm is green now, not that this lane's changes are what kept it green.

## 1. The oracle JDK is not where the handoff says it is

`HANDOFF-20260819.md` opened with:

> HotSpot 25.0.3+9-LTS at `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`
> is the oracle.

**Measured, this host, 2026-08-20:** `C:/Program Files/Eclipse Adoptium/` does
not exist. `command -v javap` resolves to
`C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot/bin/javap`. That image is the
same `25.0.3+9` build and its `lib/src.zip` carries both `java.base/…` and
`java.management/…` entries — verified by listing:

```text
  62078  java.base/java/lang/Runtime.java
  28873  java.base/java/lang/ref/Reference.java
  18289  java.base/sun/nio/fs/WindowsFileAttributes.java
  77993  java.management/javax/management/ObjectName.java
```

This is not cosmetic. The handoff's §2 and §5 command blocks are the ones a new
lane copies verbatim, and both carried the dead path; two agents dispatched this
morning were handed it and had to be corrected mid-flight. Both occurrences in
`HANDOFF-20260819.md` are now fixed, with a correction note.

**Sixty-six other records in this directory still name the Adoptium path and are
deliberately left alone** — they are dated snapshots and rewriting them would
falsify what they measured. The rule that follows is the useful part:

> **Do not copy a JDK path out of a record. Resolve it.**
> `JDK="$(dirname "$(dirname "$(command -v javap)")")"`

`regression-suite/run.sh`'s own fallback (`C:/Program Files/Java/jdk-25`) does
not exist here either, which is why §5 of the handoff insists on passing `JDK`
explicitly.

**The shape, which this project keeps meeting:** a value that was true when
written, carried forward by copy rather than by re-derivation, in the one
document that is read first. Compare `feature-designs/jdk-only-mode.md` §8's
stub count (`157`), which propagated into three documents before `G83-1` caught
it. An environment path rots exactly like a measurement does.

## 2. `JMX real path` P0 row, step 1 — the two registrars are pinned

The row's remedy step 1, verbatim: *"Pin `register_object_name` and
`register_object_instance` with `register_as` (the ambient-category audit §8.3
stage 2) — a no-op on behaviour that removes the last ambient dependency in the
JMX surface."*

**Correction to the row: there is no `register_as` in this tree.** `grep -rn "fn
register_as" native-api/src` matches nothing, and `register_as(` matches nothing
in `native-builtins/src` or `native-awt/src`. What exists is
`NativeMethodRegistry::set_category` / `current_category` — the 564-site
save/restore idiom — and `with_category`. The pin is therefore expressed in the
idiom `jmx.rs` already uses at 20+ sites, which is what the row's *intent* asks
for.

Applied in `native-builtins/src/jmx.rs`:

* `register_object_name` (23 registrations) and `register_object_instance` (2)
  now open and restore their own `Bridge` window instead of inheriting one from
  `register_jmx_natives`'s.
* Both carry a `// JDK-ONLY-WAVE2:` comment naming why.

**Behaviour is unchanged and that is the point.** The only caller already opens
`Bridge` immediately before calling them, so the effective category is identical
today. What changes is that it is no longer *ambient*: the tag was right by
inheritance, and `javax/management/ObjectName` is the very class the reverted
retag NPE'd on, so "right by inheritance" was a trip waiting for the next caller
to move. Falsifiable from `--dump-native-registry`: the 25 rows must still read
`bridge`, and `registered_by` must still name these two functions.

## 3. `JMX real path` P0 row, step 2 (the "convert" half) — `ObjectName` no longer indexes a real layout by number

The row's step 2: *"Convert `ObjectName`'s index-based field access and give the
stub real field names **before** anything is dropped (the object-layout audit
§5: convert, verify, unpad, then drop)."*

Three helpers in `native-builtins/src/jmx.rs` used a hard-coded slot `0`:

```rust
fn object_name_text(...)      { match ctx.get_field(obj, 0) { … } }
fn object_name_set_text(...)  { ctx.set_field(obj, 0, Value::Object(Some(s))); }
fn object_name_new(...)       { try_alloc_concurrent_synthetic(ctx, "javax/management/ObjectName", 1)? }
```

**The real layout, `javap -p javax.management.ObjectName` on this host's JDK
25.0.3+9** (instance fields only, declaration order):

```text
  0  private transient java.lang.String                       _canonicalName
  1  private transient javax.management.ObjectName$Property[] _kp_array
  2  private transient javax.management.ObjectName$Property[] _ca_array
  3  private transient java.util.Map<String,String>           _propertyList
  4  private transient int                                    _compressed_storage
```

So slot `0` **is** `_canonicalName` today — the code was right *by coincidence*,
and nothing in the tree pinned that coincidence. Per
`docs/architecture/natives-over-real-jdk-classes.md`, a slot index against a real
layout is heap corruption rather than a wrong answer the moment the order moves.

Applied: a new `object_name_text_slot()` resolves `_canonicalName` by NAME via
`resolve_field_index_by_class_id(class_id_of_object(obj), …)`, and both
accessors use it.

**The fallback to slot 0 is deliberate, and it is not a defaulting reader hiding
a wrong write.** `NativeContext::set_field_by_name` is documented as a **no-op
when the field is not found**, so switching blindly to the by-name *setter*
would have silently dropped every write on the synthetic single-slot carrier
that `object_name_new` fabricates when no real `java.management` image is
present. Resolving the *index* and falling back only on `None` keeps both
carriers correct and keeps the write observable in either.

**Also established while reading it:** the row's claim that *"any write past slot
0 is silently discarded"* is **stale for the allocation half**.
`try_alloc_concurrent_synthetic` already widens the allocation to
`max(real_instance_field_count, requested)`
(`native-builtins/src/util_concurrent_ext.rs`), with an in-tree comment naming
the KC16 bootstrap trip that motivated it — so a real `ObjectName` gets five
slots here, not one. The requested width is deliberately left at `1` so the
**synthetic** carrier is not padded to five; the object-layout audit's order is
*convert, verify, unpad, drop*, and padding the synthetic carrier moves in the
wrong direction.

## 4. What this does NOT do, said plainly

* **The `(b) Layout` half of the JMX P0 row is untouched.** `_kp_array`,
  `_ca_array` and `_propertyList` are still left null on a real `ObjectName`.
  Real bytecode reading them still NPEs. Only the canonical-name slot is now
  name-resolved. **The row does not close on this.**
* **Step 3 of the row is untouched** — running real `java.management` /
  `javax.management` bytecode and keeping only the VM-native leaves.
* **Neither change moves the `--jdk-only` shadow count.** The 25 registrations
  stay `Bridge`; nothing was retired. Per `HANDOFF-20260819.md` §1, only
  retiring `Bridge`-tagged shadows moves strict mode, and this retires none.
* The other ~40 index-based `set_field` / `get_field` sites in `jmx.rs`
  (`MemoryUsage`, `RuntimeMXBean`, the thread-info family) are **not** converted.
  They are the same species and each needs its own `javap` read.

## 5. A Rule-1 check that saved a wasted edit

`README.md` Rule 1 — *do not believe a "not applied"*. `W8-C4-2` publishes a
one-line nomination for `vm/src/runtime/interpreter/typecheck.rs` as pending
work. It is **already applied** (`typecheck.rs:1534`; `git log -S` on the literal
`java/util/ImmutableCollections$Map` names `b7e24364f`). The record's
`STATUS: NOMINATION` banner is stale.

That matters beyond the saved minute: `RImmutableFactoryTypes` is still one of
the five standing `SUITE=all` failures, and the applied guard's own comment says
why — it closes the **strict** face only, while the compatible face is the
`cratonvm/internal/UnmodifiableMap` stamp's missing `AbstractMap` chain (P4A
N1b). **A record left at NOMINATION after its patch lands invites the next lane
to "fix" it again and then wonder why the vector is still red.**

## 6. Control measurement (baseline, NOT this lane's changes)

Binary: `C:/craton/target-jdkonly-h2/release/cratonvm.exe`, built from
`26e4b5db4` with `--config profile.release.lto=false`; 21m13s, clean.
Command:

```bash
TIMEOUT=420 JDK="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot" \
  CV="C:/craton/target-jdkonly-h2/release/cratonvm.exe" \
  CRATONVM_ARGS="--jdk-only" bash regression-suite/run.sh
```

The result is recorded in the orchestrator's round-1 merge note rather than here,
because a number in this section would be superseded within the hour and this
directory has been burned three times by exactly that.

## 7. OUT-OF-FILE EDITS REQUIRED

None. All three changes are inside files this lane owns
(`native-builtins/src/jmx.rs`, `docs/known-issues/jdk-only/HANDOFF-20260819.md`,
and this record).

## 8. NOMINATIONS

* **N1 — `docs/jdk-only-runtime-services.md`, the `JMX real path` row:** delete
  the `register_as` prescription or rename it to the API that exists. A remedy
  naming a function that has never existed reads as "not done yet" forever.
* **N2 — the remaining `jmx.rs` slot indices.** ~40 sites across `MemoryUsage`
  (4), `RuntimeMXBean` (8+), `ThreadInfo` and the lock family. Each is one
  `javap` read away from a name. Do them **per bean, not in one sweep**: a wrong
  field NAME is a no-op and a wrong INDEX is heap corruption, and a sweep mixes
  the two failure modes under one diff.
* **N3 — `_kp_array` / `_ca_array` / `_propertyList`.** This is the row's real
  step 2. It needs `ObjectName$Property` instances, which means the parse the
  real constructor performs. The honest sequencing is to make the native
  construct a real `ObjectName` through its own bytecode and stop fabricating,
  rather than populate five fields by hand.
* **N4 — re-status `W8-C4-2`** from `NOMINATION` to applied-with-a-live-residual,
  and point its residual at P4A N1b. See §5.
