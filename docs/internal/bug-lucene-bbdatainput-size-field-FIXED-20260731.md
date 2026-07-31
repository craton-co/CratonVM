# Lucene `ByteBuffersDataInput` shim read a field that does not exist, and `Lookup.findVirtual` never threw — FIXED 2026-07-31

## Status
**FIXED** — `fix/lucene-bbdatainput-eof-20260731`, merged to `dev`.
Retired here from `docs/known-issues/h2/bug-lucene-bytebuffersdatainput-eof-lz4-presetdict.md`.

Two independent defects, both required to make H2's `FULLTEXT_LUCENE` work.
With them fixed, **`org.h2.test.db.TestFullText` and
`org.h2.test.unit.TestRecovery` pass** — the two classes the original
`Runtime.version()` report named back on 2026-07-30.

---

## Defect 1 — the shim read `this.length`; the field is `this.size`

`native-builtins/src/lucene_es.rs` natively reimplements
`org.apache.lucene.store.ByteBuffersDataInput`. Every bounds check read the
receiver's `length` field. Lucene 9.7 declares:

```java
private final ByteBuffer[] blocks;
private final int blockBits;
private final int blockMask;
private final long size;      // <- this one
private final long offset;
private long pos;
```

There is no `length` field. `lucene_field_long` answers **0** for a missing
field, so every bounds check compared against 0 and *every single read* failed.

What made it hard to see: `size()` looked perfectly healthy. It is not
intercepted, so the real one-line getter ran and returned the real field. A
probe that only checks `size()` reports the object as fine.

Measured on `dev` before the fix — 25-check probe, 8 of 8 single-block reads
and 8 of 8 multi-block reads failing:

```
s.size=64                                     <- correct, misleading
s.readByte=THREW java.io.EOFException: Unexpected EOF
s.readInt=THREW java.io.EOFException: Unexpected EOF
s.readByteAt10=THREW java.lang.ArrayIndexOutOfBoundsException
s.slice=THREW java.lang.IllegalArgumentException: ...slice out of bounds
m.readBytesAcrossBlockBoundary=THREW java.io.EOFException: Unexpected EOF
```

**Fix:** one accessor, `lucene_bbdin_size`, naming the field exactly once; all
seven read sites and `slice`'s write of the new instance's size go through it.

The block-boundary arithmetic that the original report fingered as the prime
suspect (`lucene_bbdin_copy_abs_to_array` treating `block_offset >= limit` as
mid-copy EOF) turned out to be **correct** — it matches Lucene's own
`chunk = min(len, block.remaining()); if (chunk == 0) throw new EOFException()`.
It only ever fired because `size` was 0.

## Defect 2 — `Lookup.findVirtual` returned a handle for a missing method

With defect 1 fixed, index writes worked and H2 failed later with
`NoSuchMethodError: org.apache.lucene.search.TotalHits.value()J`.

That is H2 *version-probing* Lucene. `FullTextLucene.<clinit>`:

```java
try   { mh = lookup.findVirtual(TotalHits.class, "value", methodType(long.class)); }
catch (Exception e) { mh = lookup.findGetter(TotalHits.class, "value", long.class); }
```

Lucene 10 exposes `TotalHits.value()`; Lucene 9.7 has only the `value` field.
HotSpot raises `NoSuchMethodException` from `findVirtual` and H2 takes its
fallback. CratonVM's `findVirtual`/`findStatic`/`findSpecial` **never threw** —
they handed back a `MethodHandle` regardless and deferred the failure to invoke
time as `NoSuchMethodError`. `NoSuchMethodError` extends `Error`, so it sails
straight through `catch (Exception)` and the fallback never runs.

`no_such_method_error`/`no_such_field_error` also only *spelled* the exception:
they built a `VmError::Internal` whose message read `"NoSuchMethodException: …"`,
not a real `java.lang.NoSuchMethodException`.

**Fix** (`native-builtins/src/lang_invoke.rs`):
- Both helpers now build the real typed `RuntimeError::NoSuchMethodException` /
  `NoSuchFieldException`.
- `findVirtual`/`findStatic`/`findSpecial` throw it when the member is absent.
  The escape hatch the old "create it anyway" comment protected is kept where
  it belongs: `method_exists` already answers *true* for synthetic-stub classes
  and for anything in the native registry, so a false answer means genuinely
  absent. Only when the class itself fails to initialise do we stay quiet.
- `findGetter`/`findSetter` are left **deliberately permissive**: HotSpot throws
  `NoSuchFieldException`, but `resolve_field_index` walks only the real class
  hierarchy — no synthetic-stub or native-registry fallback — so a `None` there
  does not reliably mean "absent". Nothing observed needs the throw (H2 needs
  `findGetter` to *succeed*). Revisit when a stub-aware field predicate exists.

## Verification
Host: Azure Linux box, `--java-home /home/victor/jdk25` (Temurin 25.0.3), `--nojit`.

- `docs/known-issues/repros/lucene-bbdatainput/BBDinProbe.java` — 25 checks over
  single-block and multi-block inputs: `size`, `position`, `readByte`,
  `readShort/Int/Long`, absolute `readByte(long)`/`readInt(long)`, `readBytes`
  within a block and **across a block boundary**, whole-buffer reads, `seek`,
  `skipBytes`, `slice` (including across blocks), a short final block, and the
  read-past-end EOF contract. **All 25 match real HotSpot byte-for-byte**; the
  only throw is the one that is supposed to throw.
- `docs/known-issues/repros/lucene-bbdatainput/MhLookupProbe.java` — 11 checks
  over `find*` present/missing, catchability as `Exception`, and H2's exact
  version-probe shape against both a local class and the real
  `org.apache.lucene.search.TotalHits`. Matches HotSpot except the two
  deliberately-permissive field lookups, which are called out above.
- `LuceneMini` (open `FSDirectory`, add a document, `commit`, reopen): went from
  never returning to `DirectoryReader.open numDocs=1` with flat RSS.
- H2 `FtlRepro` / `FtlStages`: `HIT "PUBLIC"."TEST" WHERE "ID"=1`, `FTL_OK` —
  identical to real HotSpot.
- **`org.h2.test.db.TestFullText` `exit=0`, `org.h2.test.unit.TestRecovery`
  `exit=0`** (both `exit=1` on `dev`).
- No regression: an invokedynamic/MethodHandle probe (lambdas, method and
  constructor refs, string concat, streams, record `equals`/`hashCode`/
  `toString`, `VarHandle`, direct `findStatic`/`findGetter`/`findStaticGetter`)
  is identical on `dev` and after the fix, and both match HotSpot. Six
  unrelated H2 test classes A/B identical. `cargo test -p
  cratonvm-native-builtins`: 3147 passed, 0 failed.

## Lesson
Both defects are the same shape as the one that preceded them
(`Runtime.Version.build()`): **a native shim written against a different version
of the thing it shims, failing silently.** `lucene_field_long` returning 0 for
an absent field and `findVirtual` returning a handle for an absent method are
both "answer plausibly rather than admit ignorance" — and both cost far more to
diagnose than an immediate error would have. When a shim reads a field or method
by name, the name is an interface contract; state it once, in one accessor, and
make a probe assert the shape.
