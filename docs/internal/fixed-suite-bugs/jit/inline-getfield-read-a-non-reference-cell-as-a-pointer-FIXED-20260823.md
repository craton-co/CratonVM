# The inline `getfield` read a 16-byte cell's payload as a pointer without checking the cell's tag

| | |
|---|---|
| **Status** | FIXED — 2026-08-23, `jit/src/ir_lower.rs` + `jit/src/x64/bytecode_walk.rs` (three arms), `vm/src/jit/helpers.rs` |
| **Severity** | high — a wild-pointer dereference in compiled code, i.e. a VM memory-safety defect, not a wrong answer |
| **Symptom** | `SIGSEGV addr=0x5` in `org.apache.derby.iapi.types.SQLChar.readExternalFromArray` |
| **Found via** | `catalina.servlets.TestWebdavPropertyStore`, one of the two Tomcat `rc=139` crashes in the 2026-08-22 sweep |
| **Root cause** | the inline `getfield` fast path loaded the `Value` cell's 8-byte payload and handed it on as a reference; nothing looked at the cell's discriminant |

## The instruction

The compiled body of `SQLChar.readExternalFromArray` ends the inline
`getfield rawData:[C` sequence like this (offsets from the code buffer base):

```text
  1253: mov  [rbp-48h],r14        ; receiver
  1257: mov  rax,r14
  125a: test rax,rax
  125d: je   <helper>             ; the ONLY thing checked: a null receiver
  1263: mov  ecx,[rax+0Fh]        ; GC flags byte
  1269: and  rcx,4                ; GC_FLAG_COMPACT
  126d: je   127Fh
  1273: mov  rax,[rax+18h]        ; compact: the bare reference word
  127f: mov  rax,[rax+28h]        ; legacy: the 16-byte cell's PAYLOAD, at +8
  12bc: mov  [rbp-48h],rax
  12c0: mov  eax,[rax+4]          ; arraylength  ->  SIGSEGV
```

`rawData` is declared `[C`. Its cell did not hold an `Object`. The legacy arm
read the payload word anyway — `1` — and the `arraylength` four instructions
later is `MOV r32,[RAX+4]`, so the fault address is `1 + ARRAY_LENGTH_OFFSET`
= `0x5`.

Three crashes carried byte-identical registers (`rax=1 rcx=0 rsi=3 rdx=0x7ffffffff000`,
`addr=0x5`). **The compiled body was deterministic; only its reachability was
racy** — 3 crashes in 38 runs one hour, 0 in 129 the next. That distinction
decided how the fix could be verified at all.

## Why the helper never saw it

`jit_getfield` reads the same cell through `read_value_atomic` and then
`match`es on the variant, so it has always been correct here. The inline arm
exists precisely to skip the helper — and in skipping it, it skipped the
question the helper was asking. The two paths had drifted apart in the one
place where the difference is memory safety.

`J`, `D`, `L` and `[` all share the 8-byte payload offset, so the field
DESCRIPTOR cannot tell you what the cell holds. Only the tag can.

## Fix

The cell's discriminant is now compared against `FIELD_CELL_TAG_OBJECT` before
any reference arm reads the payload, and a mismatch branches to the checked
helper — three arms, all of which had the same gap:

* `ir_lower.rs`, the IR backend's inline legacy read (the one that crashed);
* `x64/bytecode_walk.rs`, the legacy fallback inside the compact-layout arm;
* `x64/bytecode_walk.rs`, the plain legacy arm — found by the regression test,
  not by reading, and it would have kept the crash alive at tier 1.

Deferring to the helper rather than degrading to null inline keeps ONE place
deciding what a punned slot means, and that place counts it. Cost is one
compare and one not-taken branch on the legacy reference path; the compact arm
is untouched.

`RAW` mode (`CRATONVM_JIT_INLINE_GETFIELD`) is excluded: it has no slow path to
defer to. It is an explicit opt-in whose own comment calls itself "historical
semantics"; the guarded path is the default and is what the crash was on.

Alongside, `jit_getfield` gained `GETFIELD_EXPECT_REFERENCE` — the JIT now
tells the helper that it will DEREFERENCE the answer, so a primitive found in
a slot the caller will follow is degraded to null instead of returned. All
seven emit sites across both backends go through one encoder
(`getfield_index_arg`) so a new flag cannot be forgotten at one arm.

## What was wrong on the way

**The first fix was the helper contract, and it was not the crash path.** It is
a real hole and it stayed in, but the crash went straight past it. Measured, not
guessed: with it in, the workload still crashed 3 times in 260 runs and the new
containment counter read **0**. A counter that says the guard never fired is
what stopped that from being reported as a fix.

**The `num_slots=0` signature sent me at compaction.** `TestSwallowAbortedUploads`'
log carries `zgc real: field index OOB index=1..4 num_slots=0`, which `zgc.rs`
documents as the fingerprint of a stale pointer into a compacted-away object.
Five heap sizes down to 192 MB and 40 runs produced neither the warning nor a
crash. It is a real signature, and it belongs to the other crash, not this one.

## Verification

**A deterministic test, because the workload cannot be one.** At 3 crashes in
38 runs and then 0 in 129 on the SAME binary, "it stopped crashing" cannot
distinguish a fix from an hour's luck.

`a_reference_getfield_does_not_read_a_non_reference_cell_as_a_pointer`
compiles a reference `getfield`, runs it against a cell punned to a `Long`, and
asserts the result reaches the marker helper. Pre-fix it returns `0x12345678` —
the payload word, which is what compiled code dereferences next. It also
asserts the two cases the fix must NOT disturb: a genuine reference still reads
inline, and `Object(None)` still reads inline as 0.

Suites, on the fixed tree: `cratonvm-jit` 2105, `cratonvm-vm` 2606,
`cratonvm-types` 583, `cratonvm-gc` 1687 — 0 failed.

**Interleaved A/B**, `TestWebdavPropertyStore`, four concurrent streams, arms
alternated per run so host drift cannot line up with one binary. Two rounds,
both reported:

| round | arm | binary | runs | `rc=139` |
|---|---|---|---:|---:|
| 1 | pre | `cratonvm-segv-pre-12e8bc367` | 462 | **3** |
| 1 | post | `cratonvm-segv-fix2` (IR arm only) | 462 | **0** |
| 2 | pre | `cratonvm-segv-pre-12e8bc367` | 356 | **0** |
| 2 | post | `cratonvm-segv-fix3` (all three arms) | 355 | **0** |

**Round 2's control did not crash either**, which is the whole point about this
workload: 818 pre-fix runs produced 3 crashes, and they do not arrive evenly.
Against a 3-in-818 base rate, seeing 0 in 817 post-fix runs is about a 1-in-20
coincidence — suggestive, not proof, and it is quoted here as such. The
deterministic test above is the evidence; this is corroboration.

The control binary was rebuilt from the parent commit and reproduced its md5
exactly, so the arms differ by the fix and nothing else.

The engagement counter is the stronger runtime signal, and it is printed beside
the result: **median 8494** `getfield reference loads that contained a primitive
slot` per run (min 2, max 9015 over 355 runs), against 56 626 total getfield
helper calls. So the punned-cell condition occurs ~8 500 times per run on this
workload and every one of them used to be a payload word handed to compiled code
as a pointer. Most were survivable — a zeroed cell's payload is also zero, which
is the right answer by accident — and occasionally the word was `1`.


**Full Tomcat suite on the fixed binary**: **615 PASS / 20 FAIL / 5 HANG, zero
CRASH** — the 2026-08-22 sweep this crash came out of had two.

Nine classes differ from the last recorded baseline (`fair-20260823`), which was
measured on a binary about eighty dev commits older, so that baseline cannot
separate this change from everyone else's. Each was therefore re-run against the
pre-fix build of THIS commit's parent, and every one behaves identically on both
arms:

| class | pre | post |
|---|---|---|
| `jakarta.servlet.http.TestHttpServlet` | 1 of 17 fail | 1 of 17 fail |
| `catalina.connector.TestCoyoteOutputStream` | 12 of 14 fail | 12 of 14 fail |
| `coyote.TestIoTimeouts` | 2 of 2 fail | 2 of 2 fail |
| `coyote.http11.filters.TestChunkedInputFilter` | 1 of 42 fail | 1 of 42 fail |
| `coyote.http2.TestAsyncReadListener` | 4 of 4 fail | 4 of 4 fail |
| `coyote.http2.TestAsyncFlush` | 2 of 2 fail, 65 s | 2 of 2 fail, 66 s |
| `catalina.nonblocking.TestNonBlockingAPI` | `rc=124` @1200 s | `rc=124` @1200 s |
| `coyote.http2.TestAsync` | `rc=124` @1200 s | `rc=124` @1200 s |
| `tomcat.security.TestSecurity2025Http2` | `rc=124` @1200 s | `rc=124` @1200 s |

`jasper.compiler.TestGenerator` is the known 868 s class and needs a cap above
600 s; it is not a defect. The rest are the census's standing set.


### Correction, 2026-08-24: what that counter actually counts

The commit that landed this called the counter "a live count of type-punned
reference slots this VM is still producing". **That was wrong**, and it is
corrected here rather than quietly.

Split into "payload word zero" and "payload word non-zero" and re-measured on
the same two classes: **20 190 hits across two solo runs, ZERO of them
non-zero**. The total is dominated by reference fields of freshly allocated
objects — a zero-filled cell decodes as `Int(0)`, and the old inline read
returned the correct null from it by accident. That is not corruption and must
not be reported as it.

The number worth quoting is `JIT_GETFIELD_PUNNED_REF_NONZERO`: the cells whose
payload word was non-zero, i.e. the ones the inline arm would genuinely have
handed to compiled code as a pointer. `SQLChar.rawData` with `payload64=1` is
one of those, and it is rare — which is consistent with the crash being rare,
and inconsistent with the "8 500 per run" reading.

## Residuals, stated

* **A cell tagged `Object` whose pointer is garbage still passes the inline
  check.** The helper degrades that (`jit_decode_ref_word` refuses anything
  below `0x1000`); the inline arm does not, because the plausibility test costs
  more than the tag compare on the path this sequence exists to make fast. Not
  observed in any run here.
* **The second Tomcat SIGSEGV, `catalina.core.TestSwallowAbortedUploads`
  (`addr=0xf`), was NOT reproduced** — ~120 runs, zero crashes. Its address is
  consistent with the same punned `1`: `1 + KIND_TAGS_BYTE_OFFSET` (14) is
  exactly `0xf`, and `CMP BYTE [recv+0Eh],0` is the object-vs-array guard this
  backend emits. That is an inference, not a measurement: the crash predates
  the full register dump, so its `rax` was never recorded.
* **Whatever writes a non-`Object` into a reference cell is untouched.** This is
  containment. The counter is the live measure of how often the VM still does
  it (the G30-1 species).
