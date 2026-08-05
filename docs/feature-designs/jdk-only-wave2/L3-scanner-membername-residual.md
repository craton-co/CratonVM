# L3 — Trace and fix the last unclassified layout rows

**Owns:** `native-builtins/src/phases_early.rs`, `native-builtins/src/lang_invoke.rs`
**Gated on:** nothing. Smallest lane — good first task.
**Conflicts:** L12 §4 may touch `lang_invoke.rs`. Land L3 first; it is a few lines.
**Effort:** S

## Goal

Three rows in item 2's table have no classification yet:

| class | slots | writes | real desc | n |
|---|---|---|---|---|
| `java/util/Scanner` | 3, 4 | `Int` | `L` | 2 |
| `java/lang/invoke/MemberName` | 4 | `Int` | `L` | 14 |

Everything else in the table has a named writer and one of four fix shapes.
These do not, only because nobody has run the tracer on them.

## Steps

1. **Trace, do not grep.** Two rounds of grepping failed to find the
   `ClassLoaders` and `URI` writers; `overlay-bt` named both instantly with
   file:line.

   ```sh
   CRATONVM_DBG=overlay,overlay-all,overlay-bt=Scanner \
     cratonvm --real-jdk --java-home $JDK -cp probes JdkOnlyCensusLoadProbe 2>&1 \
     | grep -A 12 'rust writer'
   ```

   Repeat with `overlay-bt=MemberName`. The frame you want is index 2–3; frames
   0–1 are the hunter and `set_field`.

2. **Classify against the taxonomy** before writing any code:
   * *kind 1* — our slots on a real layout → guard by a field **name** the real
     class declares;
   * *kind 2* — right field, index against the wrong class →
     `resolve_field_index_by_class_id`;
   * *kind 3* — VM-internal, no real field → side table (see L1);
   * *kind 4* — right field, wrong representation → convert (see the `Proxy`
     fix, where an `int` had to become a `Proxy$Type` enum reference).

   Getting this wrong is silent. `Proxy` looked like kind 2 and was kind 4;
   resolving its `type` field by name finds a real field that our `int` still
   must not be written into.

3. `Scanner`'s synthetic model is allocated with 3 fields
   (`phases_early.rs`), yet the census reports writes at slots 3 and 4 — so
   either the model grew or a different writer is involved. Resolve that
   discrepancy before fixing; it may be a second site.

4. `MemberName` is in `lang_invoke.rs`, next to the `VarHandle` fix. Read
   `vh_has_synthetic_layout` first — the predicate style is the one to copy, and
   its doc comment records why the field-count version was inert.

## Verification

A/B the census against the pre-fix binary on the same probe: the `Scanner` rows
go 2 → 0 and `MemberName` 14 → 0, with every other row byte-identical. Both
probes vs HotSpot in both modes. `cargo test --release -p cratonvm-native-builtins --lib`.

## Done when

Both classes are gone from the census and the record's table records which of
the four kinds each was.
