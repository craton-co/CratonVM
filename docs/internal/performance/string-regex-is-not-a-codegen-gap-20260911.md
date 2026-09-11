# `String/Regex`: the compiled code is at parity — the gap is what it calls

2026-09-11. Windows 11, JDK 25 Temurin `25.0.3+9`, both sides. Follow-on to
[`string-regex-per-element-constant-20260911.md`](string-regex-per-element-constant-20260911.md)
and
[`native-call-boundary-the-descriptor-and-the-counters-20260911.md`](native-call-boundary-the-descriptor-and-the-counters-20260911.md).

The question this answers: *the gap is still huge — is it JIT performance, and
what would make `String/Regex` work with C2?*

## It is not codegen

`probes/SrSplit.java` runs the same arithmetic twice — once through the
intercepted natives, once written by hand in pure Java so no native is crossed.
Same loop, same checksums, one process each:

| kernel | CratonVM | HotSpot | ratio |
|---|---|---|---|
| pure arithmetic loop | 0 ns | 0 ns | **parity** |
| parse digits off a `char[]` | 10.0 | 10.0 | **parity** |
| hand-written itoa into a `char[]` | 40.0 | 10.0 | 4x |
| `new byte[8]` (allocation, no native) | 150 | 10 | 15x |
| `Long.parseLong` | 260 | 20 | 13x |
| `sb.append(i).append(' ')` | 817.5 | 12.5 | **65x** |
| `substring(6)` (native + allocation) | 1165 | 25 | 47x |

Where this JIT is asked to compile actual Java it ties C2. Every slow row is a
row that leaves compiled code. That is the finding the rest of this follows
from.

## The hot method WAS missing the optimizing tier — and it was worth ~1 %

`StringRegexOnly.run` is admitted to the optimizing pipeline and then dropped:

```
[ir] admission StringRegexOnly.run(I)J: admitted to the optimizing pipeline
[ir-bailout] JIT bailout [unallocated_value]: value n16 read before a location
             was assigned (n16's home word is never written; read it from its register)
[ir] ir_lower::lower_inner refused for StringRegexOnly.run(I)J
```

So the row's hot method runs on the single-pass backend. But the only part of
the element C2 could improve is the pure-Java arithmetic, which the table above
puts at ~30 ns of ~2 900. **Getting this method into C2 is worth about 1 % of
the row.** It is still worth fixing — see item 3 — just not as a `String/Regex`
lever.

## Where the ~2 900 ns per element actually goes

Priced apart with `probes/SrPhases3.java` and `probes/SbCostSplit.java`:

| | CratonVM | HotSpot |
|---|---|---|
| `String.length()` — already a call-site intrinsic | **2 ns** | 2 ns |
| `System.identityHashCode` — one object arg, trivial body | **120 ns** | — |
| `StringBuilder.length()` | 194 ns | — |
| `StringBuilder.append(char)` | 349 ns | — |

`String.length()` and `StringBuilder.length()` do the same amount of work. The
difference between 2 ns and 194 ns is that one of them never leaves compiled
code. **The machinery for making a native cost 2 ns instead of 349 already
existed in this VM; `StringBuilder` was not on it.** That is item 2, and it is
the real answer to "what would make this work with C2": not a better C2, but
giving C2 something to compile instead of an opaque call.

## The three changes

### 1. An inline TLAB bump for `newarray` — CORRECT, AND IT BUYS NOTHING

`jit_newarray`'s own body said "unlike the `new` site there is no inline TLAB
bump in codegen for arrays, so this helper is not a slow path — it is the ONLY
path a JIT-compiled `newarray` has". `x64::objects::emit_inline_tlab_newarray`
is that bump: eight instructions, the full array header stamped before the
cursor commits, `helpers.newarray` as its slow path.

**It does not move the number, and the measurement says why my premise was
wrong.** `probes/AllocObjVsArr.java`, single-pass tier, same loop and escape
shape:

| | CratonVM | HotSpot |
|---|---|---|
| reference store alone | 34 ns | 1 ns |
| `new Object()` — inline TLAB bump since 2024, **calls nothing** | **154 ns** | 3 ns |
| `new byte[8]` — this change | **115 ns** | 5 ns |

`new Object()` pays no helper call and is *slower* than the array path. So the
~90-120 ns an allocation costs is **not the allocator call**, and removing it
was never going to help: `CRATONVM_NO_JIT_INLINE_TLAB_NEWARRAY=1` moves
`new byte[8]` by 0 % over 9 pairs. The cost is in the machinery around every
allocation SITE — the pre-safepoint blind GPR spill, the oop map, the
post-allocation OOM check — which both arms pay identically. The `new` arm
already has a `sink_alloc_blind_spill` for exactly that, and it is not enough.
**That is the next lever, and it is a different change.**

The bump is kept: it is correct, gated, parity-tested, and it removes a real
call that becomes the next term once the spill is dealt with. It is recorded
here as NEUTRAL so nobody cites it for a number it did not produce.

### 2. StringBuilder call-site intrinsics — 262 ns → 1.7 ns

`StringBuilderFieldLayout` carries `count`/`value`/`coder` the way
`StringFieldLayout` carries `value`/`coder`/`hash`, riding on that struct so it
reaches all three compile doors without a second resolver.
`StringBuilder.length()` becomes a `count` load; `append(char)` becomes a
LATIN1-guarded capacity check, a byte store and a `count` bump.

The guard is the class id, and `StringBuilder` being `final` makes it exact —
which is what keeps `StringBuffer`, whose `append` is `synchronized` and whose
`toStringCache` every mutation must invalidate, off a path that emits neither.

**The slow edge is a CALL, not an uncommon trap**, and that is the one design
decision here that is not copied from the String family. Every other intrinsic
in the file deopts on a failed guard because its guards fail on genuinely
uncommon things. `append`'s do not: a full payload is what every growing
builder reaches O(log n) times, and a UTF16 builder fails the coder guard on
every call. Deopting there would re-run the whole method in the interpreter each
time it grew. The edges go to the same `invoke_dispatch` the site would have
used — the decline-edge shape the FFM region established.

Interleaved A/B, `CRATONVM_NO_JIT_SB_INTRINSICS=1` as arm A, 7 pairs, arm order
flipped, median of per-pair ratios:

| rung | A (off) | B (on) | paired | pairs won |
|---|---|---|---|---|
| `StringBuilder.append(char)` | 261.7 ns | **1.7 ns** | −99 % | **7/7** |
| `StringBuilder.length` | 143.3 ns | **1.7 ns** | −99 % | **7/7** |
| `String.length` (control) | 1.7 | 1.7 | ±0 | 0/7 |
| `System.identityHashCode` (control) | 83.3 | 83.3 | ±0 | 2/7 |
| `StringBuilder.append(int)` (not intrinsified) | 280 | 265 | ±0 | 5/7 |
| `StringBuilder.setLength` (not intrinsified) | 236.7 | 221.7 | ±0 | 3/7 |

Independently: the compiled non-leaf dispatch count on `SbCostSplit` drops
989 003 → **593 000**, which is the 396 000 calls the two intrinsics absorbed.

**What this cost to get right, recorded because the shape recurs.** A site the
resolver registers as an intrinsic never reaches the generic
`invoke_info.push` — every such arm `continue`s above it — so an intrinsic
whose decline edge is a CALL must register a `JitInvokeInfo` of its own. It did
not, at first, and the emitter's refusal *failed the whole method*: 404 696
interpreted dispatches against 8 695, and `append(char)` at 1 091 ns against
309. Fixing the method-entry door left the OSR door still failing every
once-invoked hot loop — which is every method that door exists for. Both now
ask one shared predicate, `string_intrinsic_declines_to_a_call`, rather than a
third hand-copy; `compile_gate`'s module doc has the general form of this
complaint.

### 3. A megamorphic site may name a homeless argument's staging slot

`ir_lower.rs`'s hashed/vtable stub read each argument's home frame word with
`slot_of`. A value the register allocator kept wholly in a register has no home
word, and `slot_of` refuses rather than read whatever the last tenant left
there — correctly. But that refusal failed the whole compile, and this site is
on the megamorphic edge of EVERY virtual call the optimizing tier lowers, so one
register-resident argument anywhere in a method dropped that method to the
single-pass backend.

The staging block written immediately below already holds every argument, put
there by `gp_load_value`, which reads a register-resident value from its
register. A homeless argument therefore has a perfectly good frame address to
name. Nothing new is emitted, and only the homeless case is redirected.

Measured by flipping `CRATONVM_NO_JIT_STAGED_ARG_SLOT`:

```
lever ON  (old): [ir-bailout] unallocated_value ... ir_lower::lower_inner refused
lever OFF (new): [ir] optimizing backend produced a body for StringRegexOnly.run(I)J
```

The refusal also now names its own call site (`#[track_caller]` on `slot_of`),
because finding this one meant rebuilding with a backtrace: the doc claimed
`home_read_refusals` "names the site" and it was a count.

## Result

`bench/StringRegexOnly.java`, n = 1 000 000, all three levers as arm A, 9 pairs,
arm order flipped, **every checksum `500000500000`**:

| series | arm A (all three off) | arm B (all three on) | paired median | pairs won |
|---|---|---|---:|---:|
| quiet host | 1 696–1 843 ms | 1 448–1 489 ms | **−14.8 %** | **9 / 9** |
| host drifting 2.2 s → 6.0 s mid-run | 2 153–6 054 ms | 1 841–4 816 ms | **−16.1 %** | **9 / 9** |
| **on the merged tree**, after `origin/dev` (which also touched `ir_lower.rs`) | 2 229–2 770 ms | 1 908–2 824 ms | **−15.9 %** | 8 / 9 |

Three series, −14.8 / −16.1 / −15.9 %, on a within-binary A/B — and the second series is the
better evidence, not the worse. Its absolute numbers nearly TRIPLE partway
through, which would have made any across-series statistic meaningless; a pair
runs both arms back to back, so its ratio cancels whatever the machine was
doing during that pair. The earlier lanes' ±4–20 % spread came from comparing
two binaries across exactly this kind of drift. A lever makes both arms one
binary, which is why these numbers are worth more than those were.

Note what the three items contribute: item 2 is essentially all of it, item 3
is ~1 % here (and is worth having for every other method it unblocks), item 1 is
zero.

**A caveat on the two intrinsic rungs.** 1.7 ns/op at 600 000 ops is 1 ms —
the millisecond timer's resolution. The reading that carries them is not the
absolute number but the 396 000 vanished dispatches and the 7/7 pairs.

## Gates

- `probes/SbParity.java` (58 assertions across 8 shapes) byte-identical to
  HotSpot with the levers on AND off: the coder guard (a UTF16 builder), the
  capacity guard (at, and one past, ten capacities), a LATIN1 array forced to
  inflate, `StringBuffer` (class-id guard, `toStringCache` coherence),
  `append` returning its own receiver, and an NPE on a null builder.
- `bench/StringRegexOnly.java` checksum correct on all 18 runs of the A/B.
- `cargo test -p cratonvm-jit --lib`: **2 378 passed, 0 failed**;
  `--tests`: 15 suites, all green.
- `cargo test -p cratonvm-vm --lib`: **2 662 passed, 0 failed**.
- Both header-offset tripwires fired on this change and were answered in it —
  `layout_constant_emission_sites_are_inventoried` and
  `header_offset_emission_site_inventory_matches_the_doc`, with
  `layout-constant-hazards.md` and `x64-flag-skew-and-contracts.md` §5 updated
  in the same commit. Every new disp8 site is behind
  `emit_inline_tlab_newarray`'s own screen, so a header grown past 127 costs
  those sites their inline path rather than addressing backwards.
- `hot_files_have_no_production_panics` caught an `.expect` added to
  `bytecode_walk.rs`; it is destructured now.
- `jit_counter_block_tests::an_out_of_range_site_is_ignored_rather_than_panicking`
  was reading a process-global sum and failed under concurrent tests
  (8 121 against 8 072). It reads its own thread's block now, which is the
  reading it always meant.

## What is left, in the order the measurements rank it

* **The machinery around every allocation site, ~90–120 ns.** Not the
  allocator, not the helper call — see item 1's table. `new Object()` calls
  nothing and still costs 154 ns. Start at `emit_pre_safepoint_spill` and the
  `sink_alloc_blind_spill` that already exists for `new`.
* **`StringBuilder.append(int)`, 265 ns, and `setLength`, 222 ns.** `append(int)`
  wants inline digit rendering; `setLength` is a `count` store behind a
  truncation check. Both are the same shape as what landed here.
* **The reference-store write barrier, 34 ns against HotSpot's 1 ns.**
  Visible in `probes/AllocObjVsArr.java`'s `store` rung, and it is paid by every
  `Object[]` element store in the program.
* **`Matcher.group()`, ~1 900 ns/element**, of which most is
  `create_string_uninterned` — a `byte[]` plus a `String` per match, so it is
  the allocation-site lever above, seen from the regex end.
* **`anewarray`.** The inline bump is primitive-only; a reference element's
  size is `ref_element_size()`, which wants baking and guarding the way the
  compact-layout snapshot is.
