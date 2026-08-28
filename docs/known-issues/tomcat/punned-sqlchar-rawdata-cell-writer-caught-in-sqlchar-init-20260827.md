# `SQLChar.rawData` holds `Int(1)`: the store is a sibling constructor's `isnull = true` landing on a `SQLChar`

| | |
|---|---|
| **Status** | **OPEN**, not fixed. The writer is identified down to six candidate bodies and seven single-lever A/Bs take it to zero; the step from those to the defect is still missing. Reopened 2026-08-27 (see below); this section is 2026-08-28. |
| **Symptom** | a `[C` field (slot 1 of an 8-slot `SQLChar`) holding `Value::Int(1)` |
| **Why it matters** | `org.apache.catalina.servlets.TestWebdavPropertyStore` FAILS — Derby cannot create its database, `NullPointerException` in `StoredPage.readRecordFromArray`. Before the detector, a compiled `arraylength` dereferenced the cell as the pointer `1`: `SIGSEGV addr=0x5` |
| **The store** | `jit_putfield_int(SQLChar, field_index=1, Int(1))`, `declared_ref=Some(true)` |
| **What emits exactly that** | `SQL{Boolean,Double,Integer,Longint,Real,Smallint}.<init>()V` — each is `putfield isnull:Z` at pc 6, field index 1, value `iconst_1`. **No `SQLChar` method emits an int store to slot 1**, proven at compile time. |

## What the store is, proven at compile time

`CRATONVM_DBG_JIT_FIELD_SITES=<method substring>` prints every field site the
single-pass backend emits: method, pc, resolved slot, type tag. Over a whole
process (1 988 sites) exactly six methods emit `slot=1` with an int-category
tag and the constant 1, and all six are Derby `DataValueDescriptor`
constructors setting `isnull = true`:

```text
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLBoolean.<init>:()V  pc=6 slot=1 tag=Z
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLDouble.<init>:()V   pc=6 slot=1 tag=Z
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLInteger.<init>:()V  pc=6 slot=1 tag=Z
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLLongint.<init>:()V  pc=6 slot=1 tag=Z
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLReal.<init>:()V     pc=6 slot=1 tag=Z
[jit-field-site] putfield method=org/apache/derby/iapi/types/SQLSmallint.<init>:()V pc=6 slot=1 tag=Z
```

Every `SQLChar` site is correct and none of them is `slot=1 tag=I`:

```text
SQLChar.<init>             pc=6 slot=2 tag=I   (rawLength = -1)   pc=14 slot=7 tag=[  (arg_passer)
SQLChar.getCharArray       pc=38 slot=1 tag=[  pc=47 slot=2 tag=I  pc=52 slot=3 tag=L
SQLChar.readExternalFromArray pc=45 slot=1 tag=[  pc=68 slot=2 tag=I  pc=78 slot=1 tag=[
```

So the receiver is wrong, not the index: one of those six constructor bodies
runs with a `SQLChar` as `this`, and `SQLChar` slot 1 is `rawData`, declared
`[C`.

## Seven levers that take it to zero

300 runs each, four streams, under load; the slot-1 accessor **read count** is
printed beside each because a lever that moves it by 10x has changed the
workload rather than fixed anything.

| lever | stores / 300 | runs failed | slot-1 reads |
|---|---:|---:|---:|
| baseline (adjacent control) | 12 | 15 | 7.51 M |
| `--nojit` | **0** | **0** | 11.0 M |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | **0** | **0** | 1.06 M |
| `CRATONVM_JIT_SCALAR_NEW=0` | **0** | **0** | 7.87 M |
| `CRATONVM_JIT_ELIDE_TRIVIAL_CTOR=0` (new lever) | **0** | **0** | 7.87 M |
| `CRATONVM_JIT_DENY=DataValueFactoryImpl.getNull` | **0** | **0** | 7.87 M |
| `CRATONVM_JIT_DENY=SQLChar.<init>` | **0** | **0** | 7.87 M |
| `CRATONVM_JIT_DENY=SQL{Boolean,Double,Integer,Longint,Real,Smallint}.<init>` | **0** | 3 | 7.80 M |

Two of those are weaker than they look and should not be read as "this code is
the corrupter": denying the factory or `SQLChar.<init>` also changes **who
creates the object**, so a zero there is consistent with "no such `SQLChar` is
produced" as well as with "nothing corrupts it". The six-constructor deny does
not have that reading — those bodies emit the store itself.

`CRATONVM_JIT_ELIDE_TRIVIAL_CTOR=0` is the tightest: it disables one `if` in
`try_compile_inner` that rewrites an elidable `invokespecial C.<init>()V`
site's callee class to `java/lang/Object` so the single-pass `0xb7` codegen can
drop the call. `CRATONVM_DBG_JIT_ELIDE_CTOR=SQLChar` shows it firing:

```text
[jit-elide-ctor] caller=org/apache/derby/iapi/types/SQLChar.<init> pc=1
                 rewrote org/apache/derby/iapi/types/DataType.<init>()V -> java/lang/Object.<init>()V
```

It is also a usable **mitigation** today, at an unmeasured cost: the elision
exists because the terminal `Object.<init>` of every constructor chain
otherwise costs one `jit_invoke_dispatch` per allocation (~69 M on bintrees18).
Do not flip its default without measuring that.

## Eleven eliminations

Each is a single lever, 300 runs, against a ~12-21/300 baseline. None moved the
rate.

| lever | stores / 300 |
|---|---:|
| `CRATONVM_ZGC_RELOCATE=0` | 10 / 600 (vs 14 / 600) |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` | 20 |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | 19 |
| `CRATONVM_COMPACT_REF_FIELDS=0` | 28 |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE=0` | 18 |
| `CRATONVM_JIT_CODE_CACHE_MAX_MB=0` (nothing reclaimed) | 6 |
| `CRATONVM_DISABLE_SCALAR_REPLACEMENT=1` | 15 |
| `CRATONVM_JIT_DISPATCH_CACHE_DIRECT_ENTRY=0` | 15 |
| `--Xmx 8g` | 15 |
| `CRATONVM_JIT_DENY=SQLInteger.<init>` (one of the six) | 14 |
| a bake-time owner-identity check in `prepare_for_publication` (below) | 1, then its own control |

The `RELOCATE=0` row has a consequence beyond itself: ZGC does not move objects
in that arm, so no explanation may assume a relocated receiver.

## Four hypotheses that died on measurement

Recording these because each looked conclusive and each cost roughly half an
hour.

1. **"The executable buffer was recycled and a stale direct call landed in the
   new tenant."** `SQLChar.<init>()V` and `SQLInteger.<init>()V` really do get
   `full-compile`d at the same entry address in every hit run — and in
   **291 of 296 non-hit runs too**. Address reuse is ubiquitous and carries no
   signal. The first version of this check counted *any* duplicate anywhere and
   looked just as convincing.
2. **"The keep-alive is missing, so the callee dies under its caller."**
   `CRATONVM_DBG_JIT_PIN=1` reports `[jit-unrooted-callee]`, and it is
   **anti-correlated**: 0 of 16 hit runs, 232 of 284 non-hit runs. When the pin
   fails, `strict_callee_roots` refuses publication — that is the SAFE case.
3. **"Then the pin succeeds on the wrong artifact, because `JIT_ENTRY_OWNERS`
   is keyed by entry address."** A bake-time `(entry, owner)` record with an
   identity check at publication was implemented and measured: its refusal
   **never fired once in 300 runs**, and the defect survived it (1/300 on the
   repeat arm, after 0/300 on the first — which was luck). Reverted; the change
   is not in the tree.
4. **"The direct bind names the wrong body at compile time."**
   `CRATONVM_DBG_JIT_DIRECT_BINDS=DataValueFactoryImpl` prints every baked
   direct call with its callee triple and entry address. Cross-checked against
   `full-compile ... entry=0x...` in the same log, across every hit run:
   **no mismatch**. The binds are right when they are made.

## Where that leaves it

The store is a raw JIT-to-JIT direct call's business (`DIRECT_CALLEE_CALLS=0`
kills it), it needs the trivial-constructor elision (`ELIDE_TRIVIAL_CTOR=0`
kills it), the emitted code is correct at every site the compiler prints, and
the bind is correct at bake time. What is not yet established is how one of the
six `<init>` bodies comes to run on a `SQLChar` receiver at run time.

The next measurement should name the executing body at the fault rather than
infer it. `compiled_frames=` in the `[punned-store-jit]` report is a
conservative stack scan and reports an unnamed frame at a consistent
`entry+0x224` — consistent enough to be the return address, not certain enough
to build on. Building the frame list from a real RBP chain (a diagnostic build
with `-C force-frame-pointers=yes`) would settle in one run what four
hypotheses could not.

## The reproduction the retirement was missing: CPU contention

| load | binary | runs | punned cells / compiled stores | runs failed |
|---|---|---:|---:|---:|
| quiet host, 4 streams | `dev` + the ecj fix | 600 | 0 | 0 |
| **+ 4 CPU spinners** | the same binary | 300 | 17 | 18 |
| **+ 4 CPU spinners** | merged tree, with this page's own store watch | 300 | **12 compiled stores** | 12 |

Same binary, ninety minutes apart, for the first two rows. The retirement's
320-run arm and its 320-run same-day baseline were both quiet-host runs, and a
quiet host measures nothing here.

```bash
for i in 1 2 3 4; do (while :; do :; done) & done      # the missing ingredient
scripts/punned-cell-campaign.sh <cratonvm> <tag> 300 4
```

## Caught in the act

The compiled-store watch the retirement added — the one that closed the blind
spot this page was originally wrong about — fires on the very first contended
campaign:

```text
[punned-store-jit] class=org/apache/derby/iapi/types/SQLChar class_id=1850
  num_slots=8 field_index=1 value=Int(1) declared_ref=Some(true)
  compact_flag=false gc_flags=0x0
   2: jit_putfield_int at vm/src/jit/helpers.rs:7894
   3: <unknown>                                   <- the compiled caller
```

The door is the **int** putfield helper, called with `field_index = 1`, on a
receiver whose slot 1 is `rawData`, declared `[C`. `declared_ref=Some(true)`
says the helper knew the slot was a reference and wrote an `Int` into it anyway.

Frame 3 is unsymbolized compiled code, so the report does not name the method.
A single-lever deny does, on the merged tree, with the read counts as the
control that it is not buying its zero with timing:

| arm | compiled stores / 300 | runs failed | slot-1 accessor reads |
|---|---:|---:|---:|
| baseline | 12 | 12 | 7 577 448 |
| `CRATONVM_JIT_DENY=SQLChar.<init>` | **0** | **0** | 7 868 505 |

Denying the whole class also gives zero, but it moves the accessor read count by
10x — a different workload. Denying every OTHER compiled `SQLChar` method, all
fourteen at once, leaves the defect intact:

| denied | punned runs / 300 |
|---|---:|
| `SQLChar.readExternalFromArray` | 21 |
| `SQLChar.copyState` | 21 |
| `SQLChar.{restoreToNull,resetForMaterialization,getCharArray,getString}` | 19 |
| `SQLChar.{writeUTF,stringCompare,isNull,compare,typePrecedence,getStreamHeaderGenerator,writeExternal,getLength}` | 8 |
| all fourteen at once | 22 |
| `SQLChar.<init>` alone | **0** |

## Where the `1` comes from

Every `SQLChar` constructor opens with the same six instructions:

```text
 4: aload_0
 5: iconst_m1
 6: putfield  rawLength:I        <- field index 2, value -1
 9: aload_0
10: iconst_1                     <- the ONLY `1` in any SQLChar constructor
11: anewarray [C                 <- an allocation the inline planner refuses
14: putfield  arg_passer:[[C     <- field index 7, a REFERENCE
```

`rawLength` is written with `-1`, not `1`. The only `1` in the method is
`iconst_1` at pc 10 — the array LENGTH operand for the `anewarray` at pc 11,
whose result belongs at `arg_passer`, field index 7, through the **object**
putfield helper. What reaches the heap instead is `jit_putfield_int(this, 1, 1)`:
the wrong helper, the wrong index, and the operand the allocation should have
consumed.

`CRATONVM_DBG=jitc` on the receiver:

```text
[cratonvm-jitc] bg-direct-call BOUND org/apache/derby/iapi/types/SQLChar.<init>()V   (x12)
[cratonvm-jitc] full-compile ...<init>()V len=1362  (x3)
[ir] admission ...<init>()V: optimize=false -- the C1/fast tier was requested, not C2
[cratonvm-jitc] inline-resolve REFUSED ...<init>()V: new/anewarray/multianewarray
```

A second lever brackets it from the other side: `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`
also gives 0/300 with matched read counts. The defect needs the constructor
compiled **and** reached by a raw JIT-to-JIT direct call.

## What is eliminated

Six single-lever A/Bs, each 300 runs with spinners against a ~21/300 baseline:

| lever | punned runs / 300 | verdict |
|---|---:|---|
| `CRATONVM_ZGC_RELOCATE=0` | 10 / 600 (vs 14 / 600) | not the collector's compaction — and so no explanation may assume a relocated receiver |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` | 20 | not the inline putfield fast path |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | 19 | not inline allocation |
| `CRATONVM_COMPACT_REF_FIELDS=0` | 28 | not the compact-layout family |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE=0` | 18 | not cached-entry owner reuse |
| `--Xmx 8g` | 15 | not heap pressure |

## What the cell looks like, against a healthy one

Dumped by the same code, so this is a diff and not a layout argument. A
reference cell carries `p32 = low half of the pointer` and `p64 = the pointer`;
an int cell carries the value in both.

| slot | field | healthy | punned |
|---|---|---|---|
| 0 | `value` | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 1 | `rawData` (`[C`) | tag=4 p32=0 p64=0 | **tag=0 p32=0x1 p64=0x1** |
| 2 | `rawLength` (`I`) | tag=0 p32=0xffffffff p64=0xffffffff | **tag=0 p32=0 p64=0x650678468550** |
| 3 | `cKey` | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 4 | `_clobValue` | tag=4 p32=0 p64=0 | tag=0 p32=0 p64=0 |
| 5 | `stream` | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 6 | `localeFinder` | tag=4 p32=0 p64=0 | tag=0 p32=0 p64=0 |
| 7 | `arg_passer` | tag=4 p32=0x40b95b88 p64=0x20040b95b88 | tag=0 p32=0 p64=0 |

Byte-identical 2 ms later — the bytes were written wrong and stay wrong, not a
read caught mid-write. Note slot 7: `arg_passer` was never written at all, which
is what a constructor whose pc-14 store went somewhere else leaves behind.

*(These `payload64` words were measured BEFORE the `write_value_atomic` padding
fix described below landed, so the non-zero `p64` on slots 1 and 2 may be that
uninitialised padding rather than a wrong-offset write. The `field_index=1` in
the store report does not depend on it.)*

## What is left to do

* **Find the mis-lowering in the compiled `<init>`.** The evidence names the
  helper, the index, the value and the method. `anewarray` is the instruction
  the operand should have been consumed by, and the one the inline planner
  refuses; the `bg-direct-call` binding is the second necessary condition.
* **The instrument cannot name the compiled caller.** Frame 3 is `<unknown>`.
  Giving `jit_putfield_int` the current compiled method — the JIT frame is on
  the stack — would close the last gap without a deny bisect.
* Everything below this line is the retirement's own work and stands: three
  independent refutations of the ORIGINAL localization, both watch fixes, and
  the `write_value_atomic` padding fix. It was right about all of it. What it
  got wrong was reading a quiet-host zero as an absence — and its own
  instrument is what caught the writer once the load was there.

## The named writer is exonerated, three times over

The page localized the writer by elimination to `jit/src/ir_lower.rs`,
`Op::Store(MemKind::Int)` — "the inline heap write". Every step of that chain
fails.

### 1. The eliminating instrument could not see the population it eliminated

> `ctx.set_field` reaches that watch (`set_field` → `set_field_no_satb` →
> `set_field_no_card`), so this native did not write the cell. […] the
> single-pass backend does not even inline primitive putfields (they all go
> through `putfield_int` -> `heap.set_field`, i.e. through that watch).

**`jit_putfield_int` does not call `heap.set_field`.** Neither do
`jit_putfield_long`, `_float` or `_double`. All four resolve the cell themselves
with `jit_field_cell_ptr` and write it with `write_value_atomic` /
`write_compact_field` — no accessor, no lock, no watch. The collector-side
`CRATONVM_DBG_WATCH_PUN` trap in `ZgcRealHeap::set_field_no_card` is blind to
the entire compiled putfield family, which is exactly the population a
JIT-written punned cell comes from.

Measured on this page's own positive control (`SQLChar:2`, one run of
`TestWebdavPropertyStore`, same binary):

| store watch | hits on `SQLChar:2` |
|---|---:|
| collector-side (`punned store watch`) | 12 548 |
| **compiled-store side (`[punned-store-jit]`, new)** | **2 413** |

Roughly one write in six of the page's own positive control was invisible to the
instrument that declared slot 1 unwritten. The measured `0` was never evidence
of absence.

### 2. The named writer is not emitted in the reported configuration

`Op::Store(MemKind::Int)`'s inline arm is preceded by

```rust
if cratonvm_types::compact_ref_fields_enabled() {
    …  jit_putfield_int helper …
    return;
}
// Receiver → RAX, value → RCX.   ← the inline write the page names
```

and `compact_ref_fields_enabled()` **defaults to true**. The page's own
reproduce command sets no `CRATONVM_COMPACT_REF_FIELDS`, so that inline write
was never emitted in any run this page describes.

### 3. The page's own report contradicts it

The inline arm ends with `MOV qword [RAX + high_off], 0` — it clears the high
qword by construction. The reported cell is `tag=0 payload32=0x1
payload64=0x1`. `payload64` is not 0.

## The READ watch was partial too, and that is why this page's numbers looked clean

The 2026-08-24 read-side arm was added to `ZgcRealHeap::get_field` so the
question would "survive `--nojit`". It does — but it only ever sees the reads
that go through the **accessor**, and a compiled `getfield` does not. Measured
on the pre-fix binary, one run:

| | punned reference reads seen |
|---|---:|
| `jit_getfield_impl` counter (compiled path) | 8 709 |
| `ZgcRealHeap::get_field` read watch (`punned read watch`) | **0** |

Both watches were counting one half of their own question. A zero from either,
on its own, means "not on this path".

*(A note for anyone diffing arms: after the fixes, the same run splits 7 248
helper-side / 1 774 accessor-side — the same ~8 700 reads, routed differently
because 891 call sites changed binding. Cell contents are identical: `tag=0
payload32=0 payload64=0`, the benign never-assigned `rawData` that both paths
correctly return as null. `--nojit` on both binaries reads 0 accessor-side, which
is the control that says the split is routing and not corruption.)*

## The ambiguity in the report, now closed

The page could not decide what a non-zero `payload64` under an `Int` tag meant,
and said so: "either a writer that used the WRONG offset or padding left by the
cell's previous occupant — and the two want completely different searches."

It was padding, but not the *cell's*. `write_value_atomic` was
`transmute::<Value, [u64; 2]>`, and `Value` is `repr(u32)` with a 32-bit payload
at byte 4 and a 64-bit payload at byte 8 — so a narrow variant (`Int`, `Float`,
`ReturnAddress`, `Uninitialized`) leaves bytes 8..16 as **uninitialised padding
of the caller's stack temp**, which the store then committed to the heap cell.
Formally UB; in practice, arbitrary bits.

`value_words` (`types/src/value.rs`) now builds both words explicitly and writes
the unused half as a hard zero. `narrow_variants_zero_the_payload64_word` and
`value_words_round_trips_through_the_atomic_reader` pin it, and ZGC's legacy
`set_field` arm switched from `ptr::write` to `write_value_atomic` so it gets the
same treatment (and the tear-freedom `g1` and `gen_heap` already had).

That word is not inert: it is the word a compiled reference `getfield`
**dereferences**, and the VM's own containment argument for a primitive in a
declared-reference slot is that it is zero — "a reference field of a freshly
allocated object, whose cell is still zero-filled and so decodes as `Int(0)` —
that word is 0, i.e. the correct null, by accident". Garbage padding is what
turned that benign null into the pointer `1` and the `SIGSEGV addr=0x5`.

**Consequence for whoever meets this next:** padding is no longer an available
explanation. If the cell reappears with a non-zero `payload64`, "a writer used
the wrong offset" is now the *only* reading — decided by the report itself
rather than by argument.

## The residual this page asked to close, plus two more of the same kind

> A non-constant offset node yields slot **0**, silently. That is not the slot
> seen here (1), so the fallback is not itself the observed defect — but it is
> the same hazard class in the same expression, and worth closing regardless.

Closed, along with two siblings that were live and unmentioned. Each now
**refuses** rather than substituting, and each is **counted**, so a future zero
is a measurement:

* **`ir_lower.rs`** — `Op::Load`/`Op::Store`'s field index came from
  `match … { Op::Const(v) => v, _ => 0 }`. `lower_inner` now refuses such a
  graph outright; the arm itself deopts rather than guessing.
* **`bytecode_walk.rs`** — four `.unwrap_or((pc, 0, b'I'))` field defaults:
  **slot 0, tagged int**. This exact default has been caught corrupting a heap
  cell before — JDT's `HashtableOfInt.rehash` storing an `int[]` as
  `Value::Int(low32_of_ptr)`, which the next `put()` died on at `arraylength`.
  The fix then populated one table and left the default for every other path.
  Now a counted bail; `CRATONVM_JIT_UNRESOLVED_FIELD_SUBSTITUTE=1` restores it
  for a single-binary A/B.
* **`jit_bridge.rs`** — field resolution matches by **name alone**
  (`find_own_field` / `find_field_recursive` / `locate_field`; JVMS §5.4.3.2 says
  name *and descriptor*), while the JIT takes the type tag from the
  constant-pool descriptor. A site where the two disagree emits one field's tag
  at another field's slot — the punning shape exactly. Now refused and counted.

Also counted, because a dropped store leaves a null that looks like a program
null: `jit_putfield_object` and its three primitive siblings each return without
writing on an implausible receiver or an out-of-bounds slot, and neither exit
recorded anything.

**None of them fired on this workload.** All read 0 across both 320-run arms.

## The measurement

Both arms: `TestWebdavPropertyStore`, four concurrent streams × 80 runs,
`--Xmx 2g`, `CRATONVM_DBG_WATCH_PUN=SQLChar:1`.

| | baseline (`a6e85e87f`, same day) | fixed branch |
|---|---:|---:|
| runs | 320 | 320 |
| `OK (2 tests)` | 320 | 320 |
| non-zero exits / SIGSEGV | 0 | 0 |
| punned cells with non-zero payload | **0** | **0** |
| compiled store watch on slot 1 | (instrument absent) | **0** |
| collector store watch on slot 1 | 0 | 0 |
| accessor read watch on slot 1 | 0 | 567 641 — all `payload32=0 payload64=0` |

Positive control, same binaries, `SQLChar:2`:

| | baseline | fixed |
|---|---:|---:|
| collector store watch | **6 879** | 12 548 |
| compiled store watch | (absent) | **2 413** |
| accessor read watch | 8 211 | — |

The baseline's 6 879 is the page's own recorded number, reproduced exactly, so
the instrument is the same one and it is live.

## Why it WAS retired, and why that reasoning was sound but for one premise

Nothing here fixed the punned write, because nothing here ever saw it. What this
work establishes is:

* the writer named by the page **cannot** be the writer (three independent
  refutations, one of them from the page's own report);
* both watches the elimination rested on were **partial**, and the compiled-store
  half — the only place a JIT-written punned cell can appear — had **no watch at
  all**;
* the report's central ambiguity is **removed** rather than argued;
* the residual is **closed**, and so are two siblings of it;
* and on 2026-08-27 the cell does not appear in 320 runs on the current tree
  **or** in 320 runs on the same-day baseline, with a live positive control on
  both.

A page whose analysis is disproven and whose symptom does not reproduce against
its own control is not an open investigation — and the last clause is the one
that failed. "Does not reproduce" was measured on a quiet host, where the rate
is zero for reasons that have nothing to do with the tree. Four CPU spinners
bring it back at 4-8%.

The sentence that follows it was exactly right, though: the instruments named
the writer on the first occurrence. `[punned-store-jit]` fired on the first
contended campaign after this was written, with the class, the slot, the value
and `declared_ref=Some(true)`. Everything in this section stands except the
verdict it drew.

## How to ask again

```bash
CRATONVM_DBG_PUNNED_REF=SQLChar CRATONVM_DBG_WATCH_PUN=SQLChar:1 \
  <cratonvm> --java-home <jdk25> --Xmx 2g -cp <tomcat-suite-cp> \
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestWebdavPropertyStore
```

Four concurrent streams. Grep `^\[punned-store-jit\]` (the compiled writer, with
class, slot, value, `declared_ref`, JIT callee and ~40 symbolized frames) and
`^\[punned-ref\]` (the reader — filtered by **class substring** now, because the
benign zero-fill population reaches 567 641 hits in 320 runs and would otherwise
spend the whole 32-line budget on noise; `=1` keeps the old dangerous-payload
subset). `CRATONVM_DBG_JIT_METHOD_STATS=1` prints the refusal and dropped-store
counters.

**Arm the same watch on `SQLChar:2` first and confirm both store sides fire.**
That is a six-second positive control, and it is what would have caught the
original blind spot.

## Related

* `bg-compile-off-nulls-a-reference-argument-FIXED-20260827.md` — the sibling
  page whose investigation un-blinded this instrument. Its own defect turned out
  to be a different member of the same "a reference reads as null" family: an
  `invokevirtual` naming a **private** method, dispatched from the receiver
  instead of bound at the resolved owner (JVMS §5.4.6 / JEP 181).
* `G30-1-the-silent-reference-slot-coercion-20260817.md` — the species.
