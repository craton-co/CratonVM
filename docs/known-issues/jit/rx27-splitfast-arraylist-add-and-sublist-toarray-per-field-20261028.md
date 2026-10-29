# `ArrayList.add` and `SubList.toArray` in the split rows: not worked in rx27

Status: OPEN. Area: JIT optimizing tier (splice planning of `java/util/ArrayList`, the ARRAYLIST_ACCESS IR
prefix). Severity: MEDIUM (performance only; facts equal HotSpot). Filed by round rx27 lane splitfast,
2026-10-28, from reading the code (a residual: no code changed for it yet).

**Wave 3 (2026-10-28, pending build): item 1 IMPLEMENTED, opt-in** as `CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX`; the
engagement greps, the `icount2` arms and the batteries are in the "Round rx27 wave 3 (lane splitfast)" section of
the parent page. Retire this page once the A/B decides the switch (flip it, or record why it lost); the
`SubList.toArray` tail and `ArrayList.grow`'s `Arrays.copyOf` stay as named there.

Parent: `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md` (rows `split-comma`,
`split-comma-10`, `list`; `list` was 2.1x HotSpot by instructions in round 25, the one split row inside the gate).

## What runs

* `String.split(CIZ)` adds every field with `list.add(substring(..))`. JDK 25 `ArrayList.add(E)` is `modCount++;
  add(e, elementData, size); return true`, and the private `add(E, Object[], int)` stores the reference and bumps
  `size`, calling `grow()` when full (`Arrays.copyOf`, or `new Object[10]` for the first add).
* The optimizing tier's ARRAYLIST_ACCESS prefix (`ir_lower::Lowerer::try_emit_ir_arraylist_access_prefix`)
  answers `get(int)` and `size()` only. `add` is a reference store, so it owes the collector's store barrier; no
  prefix covers it. It is spliced when the planner admits it (budget, `grow`'s `anewarray`, which
  `CRATONVM_JIT_IR_SPLICE_ANEWARRAY` governs -- lane B's area in rx27), else it is a call per field.
* At the end, `list.subList(0, n).toArray(new String[n])`: `SubList.toArray(T[])` does
  `checkForComodification()` and `System.arraycopy(root.elementData, offset, a, 0, size)`, `Object[]` into
  `String[]`, which takes the checked inline reference copy (`CRATONVM_JIT_IR_INLINE_REF_ARRAYCOPY_CHECKED`,
  default ON since r23 wave 12) when the collector gates allow it.

Which of these stayed calls on the current tree is not known: no `CRATONVM_DBG_JITC` trace of the split compile
after j27 exists. With `CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX=1` the `substring` site no longer spends splice
budget, so `add` may splice where it did not.

## How to find out (host)

```
cd /root/rx27/p
CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX=1 CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows split-comma 2 1 2>&1 \
  | grep -E 'in java/lang/String.split\(CIZ\)|java/util/ArrayList.(add|grow)|SubList.toArray|ir-splice-refused' | sort | uniq -c
CRATONVM_DBG_JITC=1 /root/r25-bin/cvm-<tag> -cp . R24RegexSplitRows list 2 1 2>&1 | grep -E 'ArrayList|SubList|arraycopy'
```

## What would close it

1. If `ArrayList.add` stays a call (or a splice whose `grow` arm is refused) in the split compile: an IR
   prefix for `ArrayList.add(Object)` on an exact `java/util/ArrayList` with `size < elementData.length` -- the
   reference store with the same collector barrier `Op::ArrayStore` emits, `size + 1`, `modCount + 1`, `true` --
   declining to the call for a full array (growth stays the JDK body's). Or the splice rule that admits it.
2. For `list`: a per-symbol instruction profile of the row (`perf` on the fp build) to name the larger of the 17
   `add`s and the final copy before choosing between item 1 and the copy.

## `ArrayList.grow` (`Arrays.copyOf(Object[], int)`): not done, and why (wave 3b, 2026-10-28)

Two growths per `split-ws` op (10 -> 15 -> 22). An inline prefix would have to (1) allocate an `Object[newLength]`
whose tail past the copied length is NULL -- the prefixes' bumps (`concat_emit`) write every payload byte they
allocate and never zero a tail, and the TLAB is not guaranteed zeroed -- and (2) copy references into it under
each collector's rules. The one bulk reference copy with those rules, `ir_lower::try_emit_ir_ref_arraycopy_prefix`
(the unchecked and wave-11 checked arms, with the SATB / young-destination gates), is the lowering of a
`System.arraycopy` `Op::Call` node (its seven inputs and collector plan), not a standalone emitter a prefix can
call. When `Arrays.copyOf` splices, its own `new Object[n]` (`Op::NewArray`, which zeroes) and `System.arraycopy`
already take exactly those paths inline, so the lever is the splice of `grow`, not a new prefix: check on the host
with `CRATONVM_DBG_JITC=1 ... split-ws | grep -E 'ArrayList.grow|Arrays.copyOf'` and, if `grow` stays a call,
the planner's refusal reason is the work item (lane B's anewarray rules, or the budget).
