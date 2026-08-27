# `SQLChar.rawData` holds `Int(1)`: the writer needs a raw JIT-to-JIT direct call into `SQLChar.<init>`

| | |
|---|---|
| **Status** | OPEN — writer bracketed by two single-lever A/Bs, codegen defect not yet identified (2026-08-27) |
| **Symptom** | a `[C` field (slot 1 of an 8-slot `SQLChar`) holding `Value::Int(1)` |
| **Why it matters** | `org.apache.catalina.servlets.TestWebdavPropertyStore` FAILS: Derby cannot create its database, `NullPointerException` at `StoredPage.readRecordFromArray`. Before the JIT-side detector caught it, a compiled `arraylength` dereferenced the cell as the pointer `1` — `SIGSEGV addr=0x5` |
| **Rate** | ~7% of runs, and **only under CPU contention** — see Reproduction |
| **Two levers that take it to zero** | `CRATONVM_JIT_DENY=SQLChar.<init>` and `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` |

## Reproduction — the part that was missing

The rate is not "0.5–5% depending on build". It depends on **host load**, and
without load it is **zero**:

| load | runs | runs with a punned cell | runs failed |
|---|---:|---:|---:|
| quiet host, 4 streams | 600 | **0** | 0 |
| 4 streams **+ 4 CPU spinners** | 300 | 17 | 18 |

Same binary, same day, ninety minutes apart. A campaign run on a quiet host
measures nothing, and this page's previous version budgeted 300 runs per arm
without the spinners.

```bash
for i in 1 2 3 4; do (while :; do :; done) & done      # the missing ingredient
CRATONVM_DBG_PUNNED_REF=1 CRATONVM_DBG_WATCH_PUN=SQLChar:1 \
  <cratonvm> --java-home <jdk25> --Xmx 2g -c "$CP" \
  org.junit.runner.JUnitCore org.apache.catalina.servlets.TestWebdavPropertyStore
```

Four streams, 300 runs, ~10 minutes per arm. Grep `^\[punned-ref\]` — **not**
`punned-ref`, which also matches the flag-spelling banner.

## The instrument, and why the old one could not answer

`[punned-ref]` is printed by `jit_getfield_impl` **only**. So `--nojit` — the
experiment this page was written to run — removes the reporter along with the
thing it is testing. The first `--nojit` arm here read **0 in 600 runs** and was
very nearly written down as a negative. It was a blind spot, not a result.

`CRATONVM_DBG_WATCH_PUN` now reports its DENOMINATORS at exit:

```text
[WATCH-PUN] EXIT watch=SQLChar:1 accessor_reads=36735 accessor_reads_punned=1776 \
  accessor_reads_punned_nonzero=0 accessor_stores=11633 accessor_stores_primitive=0
```

* `accessor_reads` is what makes a zero mean "the cell was not there" rather
  than "nobody looked";
* `accessor_reads_punned_nonzero` applies the same zero-payload filter
  `jit_getfield_impl` does — a freshly allocated reference cell is zero-filled,
  decodes as `Int(0)`, and is the correct null by accident. Unfiltered, that
  benign population is **half a million per 300 runs** and drowns the signal.

With that, `--nojit` becomes a real negative:

| arm | runs | punned cells | slot-1 accessor reads | runs failed |
|---|---:|---:|---:|---:|
| JIT on, spinners | 300 | 25 | 1 017 626 | 26 |
| `--nojit`, spinners | 300 | **0** | **11 020 500** | **0** |

Eleven million reads of `SQLChar.rawData` through the accessor, every one
holding an `Object`.

## What the cell actually is

The whole object, dumped at detection (`[punned-ref] slot N:`), against a
healthy `SQLChar` dumped by the same code:

| slot | field | healthy | punned |
|---|---|---|---|
| 0 | `value` (`String`) | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 1 | `rawData` (`[C`) | tag=4 p32=0 p64=0 | **tag=0 p32=0x1 p64=0x1** |
| 2 | `rawLength` (`I`) | tag=0 p32=0xffffffff p64=0xffffffff | **tag=0 p32=0 p64=0x650678468550** |
| 3 | `cKey` | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 4 | `_clobValue` | tag=4 p32=0 p64=0 | tag=0 p32=0 p64=0 |
| 5 | `stream` | tag=4 p32=0 p64=0 | tag=4 p32=0x6506 p64=0 |
| 6 | `localeFinder` | tag=4 p32=0 p64=0 | tag=0 p32=0 p64=0 |
| 7 | `arg_passer` | tag=4 p32=0x40b95b88 p64=0x20040b95b88 | tag=0 p32=0 p64=0 |

The healthy dump is what makes the punned one readable, and it should have come
first: a reference cell carries `p32 = low half of the pointer` and
`p64 = the pointer`; an int cell carries the value in both. Reading the corrupt
object without that baseline is guessing at layout constants.

Read against it:

* **slot 2 holds a POINTER under an Int tag** — 0x650678468550, whose own high
  half (0x6506) is what slots 0/3/5 carry in `payload32`;
* **slot 1 holds `Int(1)`** where a `char[]` belongs;
* slots 4, 6, 7 are raw zeros — never written at all, where a healthy object has
  `tag=4` nulls.

**Byte-identical 2 ms later.** This is not a read-mid-write race; the bytes were
written wrong and stay wrong.

## What it is NOT — six eliminations, each a single-lever A/B

Baseline for all of these is ~21 punned runs per 300 with spinners.

| lever | punned runs / 300 | verdict |
|---|---:|---|
| `CRATONVM_ZGC_RELOCATE=0` | 10 / 600 (vs 14 / 600) | not the collector's compaction |
| `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` | 20 | not the inline putfield fast path |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | 19 | not inline allocation |
| `CRATONVM_COMPACT_REF_FIELDS=0` | 28 | not the compact-layout family |
| `CRATONVM_JIT_CACHED_ENTRY_OWNER_REUSE=0` | 18 | not cached-entry owner reuse |
| `--Xmx 8g` | 15 | not heap pressure |

**The `CRATONVM_COMPACT_REF_FIELDS=0` arm retires this page's own previous
conclusion.** That version said "That leaves `jit/src/ir_lower.rs`,
`Op::Store(MemKind::Int)`, which its own comment describes as 'the inline heap
write'". That arm sits behind `if cratonvm_types::compact_ref_fields_enabled()
{ ...; return; }` and the flag **defaults to ON**, so the inline store is
unreachable in a default run — and turning the flag OFF, which makes it the only
lowering for every int store, moved the rate not at all. It is not the writer in
either configuration. The premise was never checked, and it cost a page's worth
of direction.

The `RELOCATE=0` arm carries a second consequence worth stating: ZGC does not
move objects in this configuration, so no explanation that needs a relocated
receiver is available.

## What it IS — two levers, both to zero, both with matched timing

| lever | punned runs / 300 | runs failed | slot-1 accessor reads |
|---|---:|---:|---:|
| baseline | 25 | 26 | 1 017 626 |
| `CRATONVM_JIT_DENY=SQLChar.<init>` | **0** | **0** | 1 063 538 |
| `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` | **0** | **0** | 1 063 844 |

The read count is the control that matters. Denying the whole class
(`CRATONVM_JIT_DENY=SQLChar.`) also gives zero, but it moves the accessor read
count from 1.0 M to 11.0 M — every `SQLChar` body interpreted, i.e. a different
workload. Both levers above leave that number within 5% of baseline, so neither
is buying its zero with timing.

Getting to `<init>` took a bisect, and the intermediate arms are worth keeping
because three of them are individually innocent:

| denied | punned runs / 300 |
|---|---:|
| `SQLChar.readExternalFromArray` | 21 (seen by the accessor arm; the JIT reporter goes quiet because its reader is interpreted) |
| `SQLChar.copyState` | 21 |
| `SQLChar.{restoreToNull,resetForMaterialization,getCharArray,getString}` | 19 |
| `SQLChar.{writeUTF,stringCompare,isNull,compare,typePrecedence,getStreamHeaderGenerator,writeExternal,getLength}` | 8 |
| all fourteen of the above | 22 |
| `SQLChar.<init>` alone | **0** |

Note the fifth row: denying every named method **except** the constructor leaves
the defect intact. The constructor is not one contributor among several.

`CRATONVM_DBG=jitc` on `SQLChar.<init>()V`:

```text
[cratonvm-jitc] bg-direct-call BOUND org/apache/derby/iapi/types/SQLChar.<init>()V   (x12)
[cratonvm-jitc] full-compile org/apache/derby/iapi/types/SQLChar.<init>()V len=1362  (x3)
[ir] admission ...<init>()V: optimize=false — the C1/fast tier was requested, not C2
[cratonvm-jitc] inline-resolve REFUSED ...<init>()V: new/anewarray/multianewarray
```

So the shape is: a **single-pass-compiled constructor**, never inlined (it
allocates), reached by a **raw JIT-to-JIT direct call** bound by the background
compiler. Every `SQLChar` constructor opens with

```text
 6: putfield rawLength:I        <- slot 2
11: anewarray  [C               <- an ALLOCATION between two putfields
14: putfield arg_passer:[[C     <- slot 7
```

and slot 2 and slot 7 are exactly the two slots the dump shows wrong — slot 2
holding a pointer that belongs somewhere else, slot 7 never written at all.

## What is left to do

* **Identify the codegen defect.** The two levers bracket it: it needs the
  constructor compiled AND reached by a raw direct call. The first place to look
  is what the `CRATONVM_JIT_DIRECT_CALLEE_CALLS` doc comment itself names as the
  two things that had to be fixed for raw JIT-to-JIT calls —
  `Compiler::emit_post_call_rbp_republish` and `JitEntryGuard::enter_with_compiled`
  retaining precise frame metadata so `scan_compiled_frame_bands` can bound each
  raw-call frame exactly. A constructor that allocates inside such a frame is
  exactly the case where the receiver has to be a precise root.
* **One hypothesis, explicitly not measured.** ZGC does not move here, but a
  receiver the collector cannot see is a receiver it can SWEEP — after which the
  constructor keeps writing into memory a later `SQLChar` has been handed. That
  predicts the punned object is a *different* `SQLChar` than the one under
  construction and that its address was recently freed. Neither is measured.
  Do not write it into a fix.
* **Diff the two direct-call admission gates.** `direct_callee_lookup`
  (background, `vm/src/runtime/interpreter/jit_bridge.rs`) claims "Every refusal
  gate of the mutator-side `callee_compiler` is mirrored here, in the same
  order". The mutator side has at least one the background mirror does not
  (`offload_jit_gate::caller_blocks_jit_by_name`). Whether that matters here is
  unknown — but a comment asserting its own completeness is the shape this page
  has already been burned by once.

## Instruments left behind

* `CRATONVM_DBG_WATCH_PUN=<class>:<slot>` reports
  `[WATCH-PUN] EXIT accessor_reads=… accessor_reads_punned=…
  accessor_reads_punned_nonzero=… accessor_stores=… accessor_stores_primitive=…`
  at exit, and dumps every cell of the first three healthy objects of the
  watched class as well as every punned one.
* `[punned-ref] slot N:` dumps all cells at detection and again after 2 ms, so
  transient and permanent corruption are told apart in one run.
* The watch's class predicate is a latched `AtomicU32`, not a `Mutex<HashMap>`.
  The mutex version **made the defect vanish** — 14 punned cells in 600 runs
  became 0 in 600, failures 16 → 0 — because it serialised every access to the
  watched slot. An instrument that suppresses what it is watching reports a
  clean run and is worse than no instrument at all.
