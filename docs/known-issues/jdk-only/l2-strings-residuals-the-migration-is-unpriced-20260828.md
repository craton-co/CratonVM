# L2 residuals — the layout migration is landed and correct, and its price is not yet a number

**Status: OPEN, three items, none of them a correctness question.** 2026-08-28.
The correctness half is closed and recorded in the retired
`l2-strings-eighteen-defects-five-root-causes-and-the-writer-half` write-up:
747 probe rows, 0 differing lines against HotSpot `jdk-25.0.4+7` in both
`--jdk-only` and compatible mode, gates and the three regression arms green.

This page exists because that record ends with three things a reader looking for
open work in `docs/known-issues/` would otherwise never find.

---

## N1 — nobody has priced the write path

`StringBuilder.append` is among the hottest paths in this VM, and the write side
now resolves the receiver's layout per call: two field reads, plus — for a
compact receiver — a `coder` name resolution alongside the `count` one
`sb_set_count` was already paying.

What is argued, not measured:

* the in-place append arm allocates nothing and reads no whole payload, so the
  shape of the fast path is unchanged;
* the growth rule (`max(2 * old + 2, needed)`) is byte-for-byte the JDK's and
  the one it replaced, so the amortised cost is unchanged;
* `sb_set_count` gained one integer comparison on the `StringBuilder` path and a
  `toStringCache` name resolution on the `StringBuffer` path only.

**Those are arguments. There is no number.** The A/B that answers it is exact
and cheap to set up, because this branch contains its own control:
`f52fa3fa6` has the ten null-contract fixes and the `StringBuffer` retirement
but NOT the migration, so an A/B against it isolates the migration and nothing
else. Interleave ABBA and take it on an idle host or not at all — this one
carried a load average between 8 and 20 for the whole of the session that wrote
this.

## N2 — `java/lang/StringBuilder`'s own 62 rows are a candidate retirement, DECLINED

`java.lang.StringBuffer`'s 62 registrations were retired because every one of its
methods is a `synchronized` delegation to `super` or a body touching only its own
`toStringCache`/`count`. `StringBuilder`'s methods are the same thin delegations,
so the same argument applies to its 62 rows — and it would halve this family's
remaining shadow surface.

It was declined, twice over:

* it is the hottest dispatch surface in the VM and the retirement adds a Java
  frame per call, which is N1's question again and larger;
* it is the class the interpreter's `JitIntrinsic::StringBuilder*` door keys on
  (`native-builtins/src/intrinsics/mod.rs` maps `("java/lang/StringBuilder",
  "append", …)` and friends), so retiring the registrations without deciding what
  happens to that door is a change with two moving parts, not one.

Take N1's number first.

## N3 — three methods are correct because the layout is, not because anyone registered them

`chars()`, `codePoints()` and `compareTo` have **no native registration** and
never appear in a `native-shadows-bytecode` row. They are correct today only
because the payload they read directly is now the real compact `byte[]` with a
truthful `coder`. If the migration is ever narrowed or reverted they go back to
being wrong — silently, and only above U+00FF, which is the range no
happy-path probe visits.

`probes/StringBuilderShadowSweep.java`'s `direct` and `compare` sections are the
guard. The `sb compareTo pair` row in particular is the one that catches it: the
`€` versus `₭` and `€` versus `b` rows PASS on the broken build, because
truncating a UTF-16 unit to its low byte preserves the comparison's sign often
enough to look right.

**The general lesson for the campaign, which is why it is here and not only in
the closed record:** a lane's worklist is the set of triples the report names,
and the report can only name a native that EXISTS. The methods with no native at
all are invisible to it, and they are exactly the ones running real bytecode
against a layout the VM may not have. Read the class's public API against the
registrar's list, not only the report. For this family the gap was six methods;
three were broken and three were fine, and no instrument in this campaign would
have told them apart.
