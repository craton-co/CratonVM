# hibernate-orm JSON/XML function tests SIGSEGV under G1/ZGC — ATTRIBUTED: the jump table is `coerce_field_value_for_slot`'s, one frame above the reader

**Status: FIXED and CLOSED, 2026-09-01.** The last thing holding this page open
was that its eight `hs_err` files had never been attributed — §2.7.3 said so
plainly, and §0.4 had ruled symbolization impossible because both crashing
builds are gone. It is not impossible; it just needed a different key. The
faulting instruction is now named, the descriptor byte it crashed on is
decoded, `0x5B` is explained by fact rather than by inference, and the crash
reproduces on demand in a unit test — which it never did in the seven sessions
this page spans.

The faulting instruction is the `Value` jump-table load inside
**`cratonvm_gc::heap::coerce_field_value_for_slot`**, in its `b'L' | b'['`
descriptor arm, with `desc_byte == 0x5B == b'['`. The invalid `Value` it
matches on was built **one frame lower**, by the collector's own `get_field`,
and merely passed in — which is exactly why §2.3's audit of the *read* sites
for a seven-entry jump table came back empty and concluded "none of them is
likely the faulting instruction". The reader does not contain the table. Its
caller does.

That also makes §2.3 the fix for these eight files after all. Section 3 is the
new work; everything from §0 down is preserved unchanged.

---

# 3. The attribution (2026-09-01)

## 3.1 Why §0.4's "symbolization is no longer possible" was true and not final

§0.4 is correct on its own terms: an RVA is meaningless without the image it
indexes, `CRATONVM_SYMBOLIZE` against a near-miss build answers with plausible
and entirely wrong names, and neither crashing build survives. Every attempt on
this page tried to resolve `exe+0x2E9D12` — an **absolute** fact about one
binary — and there was no binary to resolve it against.

But the crash handler dumps the memory around `r10`, and `r10` *is the jump
table*. A jump table stores **table-relative** displacements. So the
differences between its entries are the differences between the code addresses
they target: a property of the function's own layout, not of where the image
was based or how it was linked.

That is the key this page needed. It is build-independent, and the page's own
evidence already proved it: the eight files come from two different builds with
every RVA shifted, and

```
deltas from the entry at R10+0:
  -0x9e, -0x9e, -0xc7, -0xb0, +0x0, +0x0, +0x2b, +0x0, -0x106, +0x2b, -0x106, -0x122
```

is **byte-identical in all eight**, across both builds. (Verified, not assumed:
`scripts/hs-err-jumptable-fingerprint.py` parses the dump out of each file and
prints the vector; the eight lines are the same line.)

## 3.2 One hit, in five independent builds

Scanning `.rdata` for a dword run with those deltas finds **exactly one match**
in each modern `cratonvm.exe` on this box — five builds spanning 2026-08-28 to
2026-09-01, at five different RVAs, and one match apiece, never two:

| binary (linked) | table RVA | the three distinct arm bodies |
|---|---|---|
| `cratonvm/target/release` (08-28) | `0x2411edc` | `0x2f7551`, `0x2f757c`, `0x2f744b` |
| `cratonvm/target-hibreactive-20260830` (08-30) | `0x244f96c` | `0x2fbdf1`, `0x2fbe1c`, `0x2fbceb` |
| `CratonVM-hashevict-20260830` (08-30) | `0x2484c40` | `0x2fbcc1`, `0x2fbcec`, `0x2fbbbb` |
| `CratonVM-recycler-20260830` (09-01) | `0x2499cc0` | `0x2fd411`, `0x2fd43c`, `0x2fd30b` |
| `CratonVM-qlog-20260901` (09-01) | `0x249ceec` | `0x2fab21`, `0x2fab4c`, `0x2faa1b` |

A 2026-08-03 build in `apps/tomcat` does **not** match, which is the expected
and useful negative: `coerce_field_value_for_slot`'s arms have been edited
repeatedly since (the `b'L'` arm's `Double` split, the `note_field_coercion_loss`
routing), and a changed arm changes the layout the fingerprint measures. The
technique identifies a function whose *shape* has not moved, not one that has
never been touched — which is why the eight files, from builds a few days apart
from each other and a month before these, still hit.

`CRATONVM_SYMBOLIZE` on two of them, against their own PDBs:

```
0x2F7551  cratonvm_gc::heap::coerce_field_value_for_slot+0x151   [gc/src/heap.rs]
0x2F757C  cratonvm_gc::heap::coerce_field_value_for_slot+0x17C   [gc/src/heap.rs]
0x2F744B  cratonvm_gc::heap::coerce_field_value_for_slot+0x4B    [gc/src/heap.rs]
--- CratonVM-recycler-20260830, a different build ---
0x2FD411  cratonvm_gc::heap::coerce_field_value_for_slot+0x151
0x2FD43C  cratonvm_gc::heap::coerce_field_value_for_slot+0x17C
0x2FD2EF  cratonvm_gc::heap::coerce_field_value_for_slot+0x2F
```

Same function, same **offsets within the function** (`+0x151`, `+0x17C`), two
independently linked binaries. The name is not an artifact of which image was
scanned.

## 3.3 The instruction, disassembled — and `0x5B` decoded

Disassembling the current build at the function's entry settles the rest:

```asm
0x2f7400  push  rsi
0x2f7401  push  rdi
0x2f7402  sub   rsp, 0x38
0x2f7406  movzx eax, r8b                     ; <-- desc_byte arrives in r8b
0x2f740a  add   eax, -0x42                   ;     't' - 'B'
0x2f740d  cmp   eax, 0x19                    ;     'B'..'[' is 26 wide
0x2f7410  ja    0x2f744b                     ;     the `_ => value` arm
0x2f7412  lea   r10, [rip + 0x211aa5b]       ;     outer table: match desc_byte
0x2f7419  movsxd rax, dword ptr [r10 + rax*4]
0x2f741d  add   rax, r10
0x2f7420  jmp   rax
...
0x2f7439  mov   eax, dword ptr [rdx]         ; <-- the Value's u32 TAG
0x2f743b  lea   r10, [rip + 0x211aa9a]       ;     inner table: match value
0x2f7442  movsxd rax, dword ptr [r10 + rax*4]   ; <-- THE FAULTING INSTRUCTION
0x2f7446  add   rax, r10
0x2f7449  jmp   rax
```

Three facts fall out, each of which the page had been guessing at:

1. **`r10` at `0x2f743b` resolves to `0x2411EDC`** — bit-for-bit the table the
   fingerprint matched. The instruction that reads it is at `0x2f7442`, exactly
   `0x10F` below the first table target, which is exactly where the crash's
   `rip` sits relative to *its* first target. The faulting instruction is this
   one.

2. **`desc_byte` arrives in `r8b`.** In all eight logs
   `rbx == r8 == r13 == 0x5B`, and `0x5B` is ASCII `[`. §2.7.2 spent a section
   proposing that `0x5B == 91` was "an array's length, read through the `shape`
   dword" and labelled it *"inference, not attribution"*. It is neither a slot
   count nor an array length: it is the **descriptor byte** of the field being
   read, held in three registers because a register allocator keeps a
   long-lived argument alive across a 26-arm switch. Decoding the outer table
   confirms the routing — `'L'` (index 10) and `'['` (index 25) are the only
   two entries that reach `0x2f7439`, and the crash carried `'['`.

3. **The `Value` arrives by pointer in `rdx`, and `rax` is literally its tag.**
   In `hs_err_pid3512`, `rdx = 0x8D769D2B80` — a stack address, the caller's
   spill slot for the 16-byte argument — and `rax = 0xEAF82DA0`, the `u32` at
   `[rdx]`. §0.3 reasoned its way to "`rax` is a `Value` discriminant" from the
   table's shape. It is that, read from that word, by that instruction.

The arm grouping §0.2 decoded — indices 0/1/3 to one body, 2/5 to another, 4/6
to a third — is the `b'L' | b'['` arm read straight off:

| discriminants | source | why they share a body |
|---|---|---|
| 0, 1, 3 (`Int`, `Long`, `Double`) | two arms, `Int(_) \| Long(_)` and `Double(_)` | both end `Value::Object(None)`; LLVM tail-merges them |
| 2, 5 (`Float`, `ReturnAddress`) | `Float(_) \| ReturnAddress(_)` | one arm |
| 4, 6 (`Object`, `Uninitialized`) | `Object(_)` and `Uninitialized` | both are `value`, unchanged |

## 3.4 What this settles: the collector correlation, mechanically

`coerce_field_value_for_slot` takes its `Value` **by value**. It cannot have
produced the invalid enum; it only consumed it. The producer is one frame down,
and the call is `GarbageCollector::get_field_as`:

```rust
fn get_field_as(&self, obj: ObjectRef, index: usize, desc_byte: u8) -> Value {
    let raw = self.get_field(obj, index);                 // <-- builds the Value
    crate::heap::coerce_field_value_for_slot(raw, desc_byte, ..)   // <-- crashes on it
}
```

So the chain is, end to end:

```
  get_field_as(obj, index, b'[')
    -> g1::get_field / zgc::get_field
         -> read_value_atomic     : transmute 16 bytes, NO discriminant check   [UB here]
    -> coerce_field_value_for_slot(raw, b'[')
         -> match value           : movsxd rax, [r10 + rax*4], NO bounds check  [SIGSEGV here]
```

and the original triage's central unexplained fact — **always G1 or ZGC, never
Generational, on two independent runs** — is now mechanical rather than
suggestive:

* `gen_heap::read_slot` had screened the discriminant since HIB-CV-32. It
  returned `Object(None)`, `coerce_field_value_for_slot` matched discriminant
  4, and the run continued. **Generational was not avoiding the corrupt cell;
  it was surviving it**, exactly as §2.1 argued from a different crash.
* `g1::get_field` and `zgc::get_field` called the unchecked
  `read_value_atomic`. They handed an invalid `Value` up one frame, and the
  first `match` on it jumped through `.rdata`.

§2.1 got the shape of this right ("the collector correlation is a property of
the *reader*, not of the collector's barriers") without being able to name the
consumer. This is the consumer.

**Why the caller is `get_field_as` and not `set_field_as`.** Both call
`coerce_field_value_for_slot`, and the source alone does not choose between
them. The crash does: a *store* receives its `Value` from the interpreter or
from compiled code, where it was constructed as a `Value` and its tag is valid
by construction. Only a *read* can present a tag that was never written as one.
`rax = 0xEAF82DA0` therefore came out of memory, which makes this the read arm.

(The frame above the fault, `exe+0x2FB43C`, is `0x1172A` past the faulting
instruction; applying the same offset to the current build's copy of the
function lands in `g1::G1Collector::get_field_raw`, with `zgc::get_field` a few
hundred bytes further on. That agrees, and it is **not evidence** — it is
exactly the near-miss symbolization §0.4 warns produces plausible and entirely
wrong answers, and the same RVA taken literally in the current build resolves
to an unrelated function. It is recorded because it agrees, not because it
decides anything.)

It also means **§2.3 was the fix for these eight files**, and disclaimed itself
too strongly. Its reasoning was:

> All three sites match with a single-variant `if let Value::Object(Some(..))`,
> which lowers to a discriminant compare rather than the seven-entry jump table
> §0.2 decoded […] So **none of them is likely the faulting instruction in
> these eight files**.

The premise is true and the conclusion does not follow. The reader was never
going to *contain* the table; it `return`s the invalid `Value` to a caller who
matches on it. Auditing read sites for the jump-table shape was looking for the
crash one frame below where it happens. §2.3 moved `heap::read_slot`,
`g1::get_field` and `zgc::get_field` onto the screened reader — which is the
first three doors of the chain above.

## 3.5 Reproduced on demand, at last

`gc/tests/corrupt_value_cell_jump_table.rs` allocates a legacy object, writes
16 bytes into slot 0 that are not a `Value` — tag `0xEAF82DA0`, `rax` from
`hs_err_pid3512` verbatim — and reads it back through `get_field_as(obj, 0,
b'[')`, the exact call the crash took, on all three collectors.

* On current `dev`: three arms, one answer, `Value::Object(None)`. Green.
* With `g1::get_field` reverted to `read_value_atomic` — one line, nothing else
  changed: **`process didn't exit successfully: exit code 0xc0000005,
  STATUS_ACCESS_VIOLATION`**.

That is the same `EXCEPTION_ACCESS_VIOLATION (0xC0000005)` as the eight files,
from the same instruction, in under a second, without a database, without
hibernate, and without waiting 83 minutes for a suite. §1's "How to re-catch
it" is retired: the answer to "reproduce the *server* condition" is that the
server was never the variable, and the mechanism reproduces in a unit test.

The file also carries the two anti-vacuity controls the page's history argues
for: `the_corrupt_cell_is_actually_read` asserts the corrupt-cell census
actually moved (so a change that stops reaching the legacy path fails instead
of passing quietly), and `a_valid_cell_is_not_screened_out` asserts a valid
cell still round-trips (so a guard that answered null for everything would be
red).

## 3.6 §0.5 item 3 is now finished, and ratcheted

§0.5 item 3 asked for "a cheap, permanent improvement independent of finding
the site: a `match` over a `Value` freshly read from an unvalidated slot should
go through a checked constructor". §2.3 did the three GC readers it could see;
§2.5 and §2.6 bounded two walks. Four unscreened `read_value_atomic` reads were
still live, all in `vm/src/jit/helpers.rs`, none of them GC code and so none of
them in any of this page's audits:

| site | what it fed |
|---|---|
| `jit_getfield`, legacy 16-byte slot | a **seven-arm `match val`** — the same shape as the crash |
| `jit_putfield_int`, `CRATONVM_JIT_PFI_TRACE` | `{:?}`, which is itself a match over the discriminant |
| `jit_putfield_ref`, SATB pre-barrier | `if let Value::Object(Some(_))`, then `satb_barrier` — a garbage pointer onto the mark queue |
| `ffm_read_long_slot` | `match { Value::Long(v) => .., _ => None }` |

All four now go through `jit_read_value_cell_checked`, the VM-crate counterpart
of `heap::read_value_cell_checked`, with its own counter
(`JIT_CORRUPT_VALUE_CELLS`) beside the shared census.

And because "audit the tree once" is how this defect got four sessions of
partial passes, `scripts/check-value-cell-reads.sh` now fails CI on any call to
`read_value_atomic` outside its own defining module. It carries a positive
control (it must find calls to the *checked* reader, or it refuses rather than
reporting a clean tree it never read) and was verified in both directions: green
on the tree as it stands, red within one line of re-introducing an unscreened
read.

## 3.7 §0.5 item 1, made cheap

§0.5 item 1 — "keep a copy of `cratonvm.exe` + `cratonvm.pdb` beside the run
log, it costs 165 MB and is the difference between a decoded stack and this
page" — was already half-answered: the crash handler grew an
`exe build id: timestamp=... size_of_image=...` line, which tells you *which of
your ten binaries* produced a report.

It now also emits

```
#  exe pdb id: <GUID><age>
```

read from the PE debug directory's RSDS record. That is the key every symbol
server and every debugger uses to find a PDB, so a crash log carrying it stays
symbolizable when the exe is gone — 40 bytes instead of 165 MB, and it survives
a `target/` clean.

Section 3.1's technique is the fallback for logs that predate both lines, and
`scripts/hs-err-jumptable-fingerprint.py` is where it lives.

## 3.8 What this page no longer owns

Every item that held it open is closed:

* the producer — closed 2026-08-22, a wrong-kind read (§2.7.1);
* the reader guards — §2.3, and now the four JIT-helper doors (§3.6);
* the marker's extent — §2.5;
* the `record_outgoing_rset_edges` walk — §2.6;
* the fourteen unbounded flat-walk callers — answered 2026-08-26;
* **the eight unattributed `hs_err` files — §3.1–§3.4**;
* **`0x5B` — §3.3, and it was neither of the two things this page guessed**;
* a reproduction — §3.5.

The one thing that is *still* honestly true from §0.4: the two crashing
binaries are gone and no PDB for them exists. That no longer matters, because
the attribution never needed one.

---

*Everything below is the page as it stood on 2026-08-26, unchanged. Its
`Status: OPEN` line is superseded by the block at the top of this file, and so
is §0.4's "symbolization is no longer possible", §2.3's disclaimer and §2.7.2's
`0x5B` inference — each is answered above and each is left here because the
reasoning that produced it is worth reading beside the answer. Everything else
stands: §0.2 and §0.3 read the jump table correctly, and §3 is what happens
when that reading is carried one frame further. The only edit is that its title
below is demoted from `#` to `##`, so this file has one top-level heading.*

---

## (preserved) hibernate-orm JSON/XML function tests SIGSEGV under G1/ZGC — the faulting instruction is decoded: a jump table indexed by a corrupt `Value` discriminant

**Status: OPEN, mechanism IDENTIFIED, still not reproducible on demand
(2026-08-22). The crash is a VM memory-safety defect, not an environmental one.
The register dump that was already in all eight `hs_err` files identifies the
faulting instruction exactly — an unchecked jump-table load `[r10 + rax*4]`
whose index `rax` is a garbage 32-bit value — and the constants around it are
IDENTICAL in all eight, across two different builds. That is a deterministic
code path reading a `cratonvm_types::Value` whose `#[repr(u32)]` tag was not a
valid discriminant. Both earlier verdicts on this page are superseded: the
"degraded database server" hypothesis (2026-08-21) and the "moving-collector
pointer safety" reading (2026-08-20).**

Section 0 below is the new analysis. The 2026-08-21 not-reproducible
investigation and the 2026-08-20 original triage are preserved after it,
unchanged, because their negative results still stand — what changes is what
they mean.

---

# 0. The faulting instruction, decoded (2026-08-22)

## 0.1 What was already in the files

Eight `hs_err_pid*.log` files under `apps/hib-suite-runner/` carry this crash:
`19784`, `25880`, `26920`, `30180`, `31008`, `34952`, `3512`, `37016` — four
`XmlFunctionTests`, four `JsonFunctionTests`, four G1 and four ZGC. They come
from **two different builds**, distinguishable by RVA: build A faults at
`exe+0x2E9D12` with the handler at `exe+0x1A90582`, build B at `exe+0x2E9292`
with the handler at `exe+0x1A82512`. Every frame RVA differs between the two by
a constant-ish delta, so they are the same source compiled twice.

Registers, all eight:

| log | GC | `rax` | `r10` | fault address |
|---|---|---|---|---|
| 19784 | g1 | `0x6F325970` | `0x7FF7569B8EB4` | `0x7FF91364F474` |
| 25880 | zgc | `0xDB5EEC10` | `0x7FF635D79014` | `0x7FF9A3534054` |
| 26920 | zgc | `0x02C101B0` | `0x7FF7569B8EB4` | `0x7FF7619F9574` |
| 30180 | g1 | `0x837B9378` | `0x7FF635D79014` | `0x7FF843C5DDF4` |
| 31008 | zgc | `0x5E089F20` | `0x7FF635D79014` | `0x7FF7ADFA0C94` |
| 34952 | zgc | `0x0270B538` | `0x7FF7569B8EB4` | `0x7FF7605E6394` |
| 3512  | g1 | `0xEAF82DA0` | `0x7FF635D79014` | `0x7FF9E1B84694` |
| 37016 | g1 | `0xD99815A0` | `0x7FF7569B8EB4` | `0x7FFABCFBE534` |

**In every one of the eight, `fault_address == r10 + rax*4` exactly.** And in
every one of the eight, `rbx == r8 == r13 == 0x5B`, `rbp == 6`, `rdi == 0`,
`r11 == 0`, `r12 == r14`. Constant operands across eight crashes, two builds,
two collectors and two test classes is not memory corruption arriving from
anywhere it likes — it is one specific instruction in one specific code path.

## 0.2 `r10` is a jump table, and its targets are inside the faulting function

The crash handler dumps memory around `r10` (it does so believing `r10` may be
a shadow-stack pointer; here it is not, but the dump is what makes this
readable). For `hs_err_pid3512` — exe base `0x7FF6339F0000`, so
`r10 = base + 0x2389014`, i.e. inside the image, in `.rdata`:

```
[R10+0x0]  = 0xFDF60E0D FDF60E0D
[R10+0x8]  = 0xFDF60E0D FDF60E38
[R10+0x10] = 0xFDF60E38 FDF60D07
[R10+0x18] = 0xFDF60CEB FDF60D07
```

Read as signed 32-bit displacements from `r10`, those eight entries resolve to
RVAs `0x2E9E21`, `0x2E9E21`, `0x2E9E4C`, `0x2E9E21`, `0x2E9D1B`, `0x2E9E4C`,
`0x2E9D1B`, `0x2E9CFF` — every one of them within ~0x150 bytes of the faulting
`rip` (RVA `0x2E9D12`). That is the x86-64 jump-table idiom LLVM emits for a
dense `match`: `mov eax,[table + idx*4]` / `add rax, table` / `jmp rax`, with
the table holding table-relative offsets into the same function.

So the faulting instruction is **the jump-table load of a `match`, and `rax` is
the value being matched**.

## 0.3 The matched value is a `Value` discriminant

A Rust `match` on an integer needs a `_` arm and LLVM emits a range check for
it. A `match` on an **enum** needs no default and gets **no range check** —
LLVM is entitled to index the table directly, because a valid enum's
discriminant is in range by construction. An unchecked `base + idx*4` load is
therefore a `match` over an enum whose in-memory tag was invalid.

`cratonvm_types::Value` is `#[repr(u32)]` with seven variants and explicit
discriminants `0..=6` (`types/src/value.rs`), tag as a `u32` at byte 0 — the
crate's own doc table says so, and `ValueLayout` pins it. Seven arms is the
table size; a `u32` tag loaded into `eax` is the index; a garbage tag spanning
the full `u32` range is what all eight `rax` values are. The grouping visible in
the decoded table (indices 0/1/3 to one body, 2/5 to another, 4/6 to a third)
is the shape of a `match` that treats several `Value` variants alike.

`rbp == 6` in all eight is worth noting beside that: `6` is
`Value::Uninitialized`.

## 0.4 What this rules in and out

**Rules OUT the 2026-08-21 verdict.** "A degraded Postgres" cannot produce a
constant register profile across eight crashes. A SIGSEGV at a fixed
instruction with fixed operands is a code defect; the database's state can at
most decide whether the path is reached.

**Rules OUT the 2026-08-20 reading, as stated.** The original page inferred
"moving-collector relocation" from the always-G1-or-ZGC-never-Generational
pattern. The pattern is real and still needs explaining — a `Value` read that
only G1 and ZGC perform is the obvious candidate (SATB / concurrent-mark slot
scanning and the load barriers exist on those two and not on the generational
collector) — but relocation of a live object is not what the instruction says.
What the instruction says is: *sixteen bytes were read as a `Value` and were
not one.*

**Symbolization is no longer possible for these files.** Both crashing builds
are gone. Verified rather than assumed: `CRATONVM_SYMBOLIZE=0x1A90582,0x1A82512`
was run against all ten surviving `cratonvm.exe` binaries on this box
(including `CratonVM-sbjsp-20260819`, whose md5 `9ee1303259b0df1904ce9d74660c2239`
is the one the 2026-08-21 section records for its own rebuild of `509710ba8`);
not one resolves those RVAs to the crash handler, so not one is either crashing
build. Symbolizing against a near-miss build produces a plausible-looking and
entirely wrong answer — `0x2E9D12` resolves to `field_layout::compact_object_body_size`
on the sbjsp binary, and the whole 19-frame stack around it symbolizes to
unrelated functions with four-digit offsets, which is how you can tell.

## 0.5 How to catch it, and what to fix regardless

The reproduction advice in section 1's "How to re-catch it" still stands, with one correction:
**the trigger is not the database**, so re-running the full suite to "degrade
Postgres" is not the lever it was thought to be. What to do instead:

1. Any run that reproduces this must capture the `hs_err` **and the binary**.
   The single reason this page could not be closed on its own evidence is that
   nothing recorded which build produced the RVAs. Keeping a copy of
   `cratonvm.exe` + `cratonvm.pdb` beside the run log costs 165 MB and is the
   difference between a decoded stack and this page.
2. The defect is a `Value` read from memory that is not a `Value`. The
   candidate readers are the ones G1 and ZGC have and the generational
   collector does not; `gc/src/satb.rs`, `gc/src/concurrent_mark.rs` and the
   ZGC load barrier are where to look, and the question to ask of each is
   whether any of them can read slot `n` of an object whose real slot count is
   below `n` (the constant `0x5B == 91` in `rbx`/`r8`/`r13` is the right size
   for a slot index or field count).
3. A cheap, permanent improvement independent of finding the site: a `match`
   over a `Value` freshly read from an unvalidated slot should go through a
   checked constructor, so an invalid tag becomes a diagnosable VM error at the
   read rather than an unchecked jump through `.rdata` a hundred instructions
   later.

An attempt WAS made this session to recreate the "degraded server" condition
directly — the seven classes re-run against a live Postgres 16 container while
a background loop called `pg_terminate_backend` on every backend every 3 s. It
produced 7/7 PASS and zero `PSQLException`s, i.e. the disruption never reached
the test's own connections; it is recorded here as not-yet-attempted rather
than as a negative result.

---

---

# 2. 2026-08-24 — §0.5's "fix regardless" is now done for the last three readers, and §0.3's mechanism was independently confirmed elsewhere

Two things happened to this page's analysis without this page being told.

## 2.1 The mechanism was confirmed, with the same arithmetic, from a different crash

`gc/src/heap.rs`'s `read_value_cell_checked` carries a doc comment describing
**exactly** §0.3's mechanism, derived independently from a Spring Boot crash
(`JsonMarshallerTests`) rather than from these eight `hs_err` files:

> ZGC and G1 transmuted it and handed the result to a `match`, whose jump-table
> load is `[table + disc*4]` with no bounds check because Rust guarantees an
> in-range discriminant — so the low word of a heap pointer became the index.

and it names the same register arithmetic this page decoded — table in `r10`,
bogus discriminant in `rax`, faulting address `r10 + rax*4`. Two crashes, two
suites, two investigations, one mechanism. §0.3 is not a lone reading of eight
files any more.

It also supplies the piece §0.4 could only infer. The reason the pattern is
**always G1 or ZGC, never Generational** is not a `Value` read those two
collectors uniquely perform — it is that `gen_heap::read_slot` had *screened the
discriminant since HIB-CV-32* while the other legacy-cell readers had not.
Generational was not avoiding the corrupt cell; it was **surviving** it, returning
null and logging, while G1 and ZGC transmuted and jumped. The collector
correlation is a property of the *reader*, not of the collector's barriers.

## 2.2 The producer is still open, and the guard does not touch it

> **WRONG — corrected in §2.7.1.** The producer was closed on 2026-08-22, before
> this section was written; it was a wrong-KIND read (an array read through the
> flat-object path), not a stale reference, and not a GC defect. This section is
> left in place because it is what the guard's own comment said and the mistake
> was to repeat it without checking.

Worth stating plainly, because it is easy to misread the above as a fix:
screening the read does **not** repair whatever writes two heap pointers into a
16-byte cell. That producer — a live object reclaimed and its storage re-served —
is a separate, still-open defect, present under Generational too, "where this
guard is the only reason its green looks clean". This page stays OPEN for that
reason. What changed is that the same corrupt cell is no longer a localizable
diagnostic on one collector and an unrecoverable crash on the others.

## 2.3 The three readers §0.5 item 3 had not reached

`heap::read_slot`, `g1::get_field` and `zgc::get_field` were moved onto the
checked reader. Three legacy-cell readers were not, and all three are on
**G1/ZGC-only marking paths** — the exact collectors in this page's title:

| site | path |
|---|---|
| `gc/src/concurrent_mark.rs` `scan_object` | concurrent marker's 16-byte `Value` slot loop |
| `gc/src/g1.rs` `for_each_object_reference` | legacy-object reference walk |
| `gc/src/g1.rs` `concurrent_mark_step` | G1 concurrent mark |

Each called `cratonvm_types::read_value_atomic`, which loads two words and
`transmute`s them into a `Value` with **no discriminant check** — the unchecked
half of the pair whose checked half (`read_value_checked` / `read_value_atomic`'s
screened sibling) already existed and is what §0.5 item 3 asks for. All three now
call `heap::read_value_cell_checked`, so a corrupt cell decodes to
`Value::Object(None)`, is skipped by the `if let` that follows, and is counted by
the cell census instead of becoming a `Value` that is UB the instant it exists.

`cargo test -p cratonvm-gc --release`: **1687 passed, 0 failed.**

**What this is NOT.** All three sites match with a single-variant
`if let Value::Object(Some(..))`, which lowers to a discriminant compare rather
than the seven-entry jump table §0.2 decoded (targets grouped 0/1/3, 2/5, 4/6).
So **none of them is likely the faulting instruction in these eight files**, and
this change should not be recorded as having found it. What it removes is real
but different: the UB of constructing an invalid `Value`, and the marker's
ability to push a garbage pointer onto the mark queue that a later `scan_object`
dereferences as an `ObjectHeader` — a memory-safety hole `concurrent_mark.rs`'s
own comment already worried about for the *tearing* case while leaving the
*invalid-tag* case open.

## 2.5 §0.5 item 2 is ANSWERED for the concurrent marker: yes, via TOCTOU on the header

§2.4 left item 2's audit question open — *can a G1/ZGC reader visit slot `n` of
an object whose real slot count is below `n`?* For
`concurrent_mark::scan_object` the answer is **yes**, and the route is not a
missing bound but a second read of a racy header.

The function does validate an extent. `concurrent_mark_object_size` reads a
`ConcurrentMarkHeaderSnapshot`, cross-validates kind tag / element tag /
gc-flag universe / size arithmetic, and returns
`total_size = HEADER_SIZE + num_slots * SLOT_SIZE`; `scan_object` then requires

```rust
old_gen.contains(obj_ptr + total_size - 1)      // last byte is inside old-gen
```

and bails otherwise. That is a real check. **`total_size` is then never used
again.** The slot loop re-read the header:

```rust
let num_slots = (header.num_slots() as usize).min(1 << 24);   // BEFORE
for slot_idx in 0..num_slots { /* read a 16-byte Value */ }
```

So the count that was validated and the count that was walked are two separate
loads of a header this very module treats as untrustworthy — the snapshot
reader exists *because* a header can be torn or garbage, and
`ConcurrentMarkHeaderSnapshot::read` deliberately uses unaligned per-field
loads for that reason. Nothing makes the second load agree with the first. If
it is the larger of the two, the loop reads slots beyond the bytes
`old_gen.contains` approved, and `min(1 << 24)` does not help: it is a
plausibility clamp with a reach of 16 M slots — 256 MB — not an extent.

Fixed by inverting the arithmetic that was already validated, so the walk
cannot outrun the checked bytes by construction:

```rust
let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;   // AFTER
```

The `1 << 24` clamp is unchanged in effect — it is enforced inside
`concurrent_mark_object_size`, which returns `None` for anything larger and so
exits through the guard above. `cargo test -p cratonvm-gc --release`: **1687
passed, 0 failed.**

### What this does and does not settle

It removes a mechanism by which the marker could read past an object and hand
whatever it found to `read_value_cell_checked` — which, since §2.3, screens the
discriminant, so the pairing is now "bounded read, screened decode" rather than
"unbounded read, unchecked transmute". Those two changes are complementary and
neither subsumes the other.

It is **not** a demonstration that this is what produced the eight `hs_err`
files. No reproduction exists (§1), the crashing binaries are gone (§0.4), and
the faulting instruction is a multi-arm jump table that none of the sites
touched here contains. It is one concrete answer to one of §0.5's audit
questions, on one of the three readers.

**Still unanswered from §0.5 item 2:** `g1::for_each_flat_object_reference`
iterates `first_index..header.num_slots()` with no validation *in the function
at all* — it is a helper that trusts its caller, and its callers have not been
audited. That is the obvious next piece of the same audit, and it was left
alone here rather than changed blind. The `0x5B == 91` constant in
`rbx`/`r8`/`r13` also remains unexplained.

> **CLOSED 2026-08-26.** All fifteen callers have now been audited, the helper
> refuses an `Array` header outright, and its uncapped entry point is named
> `for_each_flat_object_reference_trusting_header` so the fourteen that still
> trust the count say so. See
> `internal/fixed-bugs/what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`.
>
> That work also found the reader THIS section's pass missed: the semi-space
> collector (`gc/src/gc.rs`) had the identical validated-then-re-read TOCTOU in
> three scan arms, with an unchecked `Value` transmute beside it. Both are fixed
> with the identical fix — see §6 of that page. `0x5B == 91` is still
> unexplained; §2.7.2 has the only candidate.

## 2.6 §0.5 item 2, second reader: `g1` clamped the array walk and left the flat walk beside it unclamped

§2.5 answered the audit question for the concurrent marker and left
`g1::for_each_flat_object_reference` open, because it validates nothing itself
and its 15 callers had not been read. They have been now, and one of them
already contains the argument for the fix — applied to the wrong half.

`record_outgoing_rset_edges` is hardened. It screens its seed with
`candidate_header_is_plausible`, and then clamps the walk:

```rust
// `array_length` is a u32 bounded only by `i32::MAX`, so `HEADER_SIZE + len * 8`
// is not implied to be inside the region by the header being plausible.
let walkable_elements = /* holder_walkable_slots(..) */;
```

That reasoning is correct and it is **not array-specific**. `num_slots` is a
header field of the same kind, bounded by nothing a plausible header
guarantees, and the legacy walk strides `SLOT_SIZE` = 16 bytes — so it leaves
the region **twice as fast** as the array path that was thought to need the
clamp. The array branch got it; the `else` branch one screen below handed the
object to the uncapped `for_each_flat_object_reference` and walked
`0..header.num_slots()`.

This is the reader in the crash this file records two dozen lines above:

```
collect_garbage -> retry_after_evacuation_failure -> record_outgoing_rset_edges
  -> for_each_flat_object_reference   (faulting on a 4 MiB-aligned address
                                       well past the committed arena)
```

The clamp existed, in the same function, guarding the sibling branch.

### The fix

* `holder_walkable_slots` computed `room = (end - addr - HEADER_SIZE) / 8` — a
  hard-coded 8-byte stride, which is why it could not serve the flat walk at
  all. It now takes a `stride`; its three existing call sites pass `8` and are
  unchanged in behaviour.
* `for_each_flat_object_reference_capped` bounds the legacy loop by
  `max_slots`. The original entry point delegates with `usize::MAX`, so the
  other **14** call sites are byte-for-byte identical — this deliberately does
  not re-bound walks whose callers have not been audited.
* `record_outgoing_rset_edges`'s `else` branch now derives its bound exactly as
  its array sibling does, with `SLOT_SIZE` as the stride.

`cargo test -p cratonvm-gc --release`: **1687 passed, 0 failed.**

### Scope, stated honestly

One caller of fifteen is now bounded — the one with a recorded crash. The other
fourteen still walk on `header.num_slots()` alone, and most of them do not hold
region geometry, so bounding them is a larger design question (what does a
walker without a region do with an implausible count?) rather than a
mechanical edit. They were left alone on purpose; `usize::MAX` through the
delegating entry point makes that a *visible* default rather than an implicit
one.

> **2026-08-26.** That design question was answered. The delegating entry point
> is now spelled `for_each_flat_object_reference_trusting_header` (grep for the
> old name and you will find only this page's history), the shared body refuses
> an `Array` header outright, and the fourteen stay uncapped as a written-down
> decision rather than an omission —
> `internal/fixed-bugs/what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`.

And as with §2.5: this is not a demonstration that this walk produced the eight
`hs_err` files in this page's title. It is the fix for a *different*, recorded,
crash that arrives through the same helper, plus the removal of one more way a
G1 walk can read past an object.

## 2.7 CORRECTION: the producer is FIXED, and `0x5B == 91` has a candidate

Two of this page's standing open items move, and one of them corrects §2.2.

### 2.7.1 §2.2 is wrong: the producer was closed on 2026-08-22

§2.2 said the producer — whatever writes two heap pointers into a 16-byte
`Value` cell — "is a separate, still-open defect". It is not open, and it was
already closed when §2.2 was written; the section simply repeated
`heap.rs`'s comment without checking.

`internal/fixed-bugs/corrupt-value-cell-producer-was-a-string-array-FIXED-20260822.md`
closed it, **and it is not a GC defect at all.** The VM-side half of the guard
(`CRATONVM_DBG_CORRUPT_CELL`, which can see the Java frame the collector cannot)
named the receiver in one run:

```text
obj=0x20040a803f8  slot_index=0
raw0=0x0000020040775828  raw1=0x0000020040797b78
receiver_class=java/lang/String   receiver_kind=Array   receiver_fields=2
holder=frame#69 Metadata$MetadataItemCondition.withDefaultValue pc=36 local[1]
```

`receiver_kind=Array`. The 16 bytes were never a `Value` because they were never
a *flat object's slot* — they were **array payload**, read through the
flat-object path. The reference was not stale; the read was of the wrong kind.

That also retires §0.4's remaining puzzle. §0.4 said the always-G1-or-ZGC
pattern "is real and still needs explaining" and proposed a `Value` read unique
to those collectors. §2.1 already showed the correlation is a property of the
*reader* (Generational screened since HIB-CV-32, the others did not). With the
producer identified as a kind confusion rather than anything collector-specific,
there is nothing left for a collector-specific mechanism to explain.

### 2.7.2 `0x5B == 91` — a candidate, from the same fact

§0.5 item 2 noted `0x5B == 91` in `rbx`/`r8`/`r13` and called it "the right size
for a slot index or field count". There is a specific reason a *wrong-kind* read
produces exactly that:

**`NUM_SLOTS_OFFSET == ARRAY_LENGTH_OFFSET == 4`** — they are the same `shape`
dword, asserted in `types/src/heap_types.rs`
(`the_shape_word_took_over_the_identity_hash_offset`).

So when an array is read as a flat object, `header.num_slots()` does not return
garbage. It returns the **array's length**. On the reading in §2.7.1 — an array
misread as an object — a `num_slots` of 91 is an `array_length` of 91, and the
walk then strides `SLOT_SIZE` (16) across payload whose real element stride is
8, 2 or 1, running off the object at roughly twice to sixteen times the rate the
header implies.

**This is inference, not attribution.** No binary survives, so it cannot be
confirmed against these files, and 91 could still be a field count or an
unrelated index. What has changed is that `0x5B` is no longer unexplained: there
is a documented mechanism that produces exactly a plausible mid-sized count from
a header that was never a flat object's, and it is the same mechanism as the
identified producer.

### 2.7.3 What this page still owns

* Not the producer (2.7.1), not the reader guards (§2.3), not the marker's
  extent (§2.5), not the recorded `record_outgoing_rset_edges` crash (§2.6).
* Still open: **these eight `hs_err` files have never been attributed.** Every
  fix above is a mechanism removed, not this crash reproduced, and §0.4's
  finding stands that symbolization is impossible without the binaries.
* ~~Still open as a *design* matter, now with its own page: fourteen callers
  of the flat walk remain unbounded.~~ **ANSWERED 2026-08-26** —
  `internal/fixed-bugs/what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`.
  The census the note here described (`FLAT_WALK_GIVEN_ARRAY`, "counts and warns;
  it deliberately does not refuse") rested on a premise that turned out to be
  false — every one of the fifteen callers DOES pre-branch on kind — so the walk
  now REFUSES an array header and counts the refusal (`FLAT_WALK_REFUSED_ARRAY`,
  observed zero). The fourteen remain unbounded by decision, written down, and
  visibly so at each call site.

**This page is now close to retirable.** What holds it open is one honest gap —
eight unattributed crash files — and not any known-live defect.

## 2.4 What is left for whoever reopens this

1. The producer (§2.2) — the stale-receiver defect that puts two heap pointers
   in a `Value` cell. That is the actual bug; everything above is containment.
2. If a crash with this signature recurs, it should now be *rarer and
   better-labelled*: the census counts corrupt cells and the guard records the
   slot address and both raw words. §0.5 item 1 still stands and is still the
   single most valuable thing to do — **keep the binary next to the `hs_err`**.
3. §0.5 item 2's audit question (can a G1/ZGC reader walk past an object's real
   slot count?) is answered for the concurrent marker in **§2.5** — yes, by
   TOCTOU on the header, now fixed — and in **§2.6** for `g1::for_each_flat_object_reference`'s
   crash-path caller. Its other fourteen callers were audited on 2026-08-26 and
   the question is closed for all of them, plus for the semi-space collector
   this page's own pass had skipped:
   `internal/fixed-bugs/what-should-a-walker-do-with-an-unvalidated-header-count-FIXED-20260826.md`.
   `0x5B == 91` has a candidate explanation in §2.7.2 (an array's length, read
   through the `shape` dword that `num_slots` and `array_length` share).

# 1. The 2026-08-21 not-reproducible investigation, preserved

Its negative results all stand. Its VERDICT ("the trigger is environmental")
is superseded by section 0.

## The decisive result

`509710ba8` is the `dev` HEAD both crashing runs used. Rebuilt it from source
(`md5 9ee1303259b0df1904ce9d74660c2239`) and re-ran all 7 classes through the
same `run-hib.sh` argfile and wrappers, against the same live Postgres:

| binary | G1 | ZGC |
| --- | --- | --- |
| `509710ba8` — **the crashing runs' own commit** | 7/7 pass | 7/7 pass |
| current `dev` `5b606e85e` | 7/7 pass | 7/7 pass |

`ok=N failed=0` on every one, and **`found`/`ok` match the HotSpot control
class-for-class** (3, 5, 4, 5, 3, 34, 8), so these are real passes and not a
suite that quietly ran fewer tests.

Same commit, same host, same harness, same database server, opposite outcome.
Whatever produced the crash was not in the VM revision.

## What that rules out

**The code.** Rebuilding the crashing runs' own commit is the control the
original page never ran. It passes. So the range `509710ba8..5b606e85e` did not
"fix" anything here, and no bisection of that range is worth doing.

**The "moving-collector pointer safety" inference.** The original page reasoned
from the always-G1-or-ZGC-never-Generational pattern to a relocation hazard.
`git diff --stat 509710ba8..5b606e85e -- gc/src` is **empty** — not one line of
collector code changed across the range. The inference was never supported by
anything but the pattern, and the pattern is now unreproducible.

**The JIT.** `--nojit` passes. So do both of the range's new switches
(`CRATONVM_JIT_COMPILED_LDC_CONST_CACHE=0`, `CRATONVM_JIT_LOCAL_HANDLERS=0`) —
neither brings the crash back, on either binary.

**The harness invocation.** Reproduced through the literal
`cratonvm-g1-wrapper.sh` / `cratonvm-zgc-wrapper.sh` (`-XX:+UseG1GC`, which
normalizes to the same collector as `--XX:UseGc=G1`), not a hand-built command.
Passes either way.

**A database that isn't there.** Pointing `hibernate.connection.url` at a
non-existent database yields `found=3 started=0 ok=0 failed=0` and `rc=0` on all
three collectors *and on HotSpot* — a clean skip, no crash. An unreachable DB is
not the trigger.

**HotSpot** (the original page's step 4, never done): passes all of them, same
counts. So there was never a HotSpot-vs-CratonVM divergence recorded for these.

## What changed, and the standing hypothesis

The Postgres container was restarted **after** both crashing runs and before
these:

```
ZGC rerun finished    2026-08-20 22:02
G1  rerun finished    2026-08-20 22:53
postgres StartedAt    2026-08-21 00:29:59Z   (RestartCount=0 — stopped and started by hand)
```

Both crashing runs therefore ran against a Postgres instance that had just
absorbed a 4548-class, 6-way-concurrent full suite (245 minutes) and was never
restarted in between. The isolated rerun's own log carries 68
`PSQLException`/`SQLException`/`FATAL`-class lines.

So the standing hypothesis is **a degraded database server state, not
concurrency of the client**. That is consistent with the original page's finding
that removing client-side concurrency changed nothing — the second run was
isolated, but it pointed at the *same un-restarted server*, so it did not
actually vary the thing that mattered. "Full isolation rules out contention" was
the wrong conclusion from a run that held the real variable fixed.

Note this does **not** excuse the VM: a SIGSEGV is never an acceptable response
to a misbehaving database. If the trigger can be recreated, there is very likely
a real defect on that error path. It simply has not been demonstrated yet, and
it is not where the original page pointed.

## How to re-catch it

Do not re-run the 7 classes in isolation — that has now been done eight
different ways and always passes. Reproduce the *server* condition:

1. Run the full 4548-class suite 6-way concurrent against a fresh Postgres, as
   `full-pg-*-20260820-3gc-pg-v2` did.
2. **Without restarting Postgres**, immediately re-run just these 7.
3. If they crash, capture `CRATONVM_SYMBOLIZE=<RVA>` against that exact binary
   *before* touching the container — the symbolized frame is the whole ask, and
   it is unobtainable once the server is restarted.
4. Record `docker inspect -f '{{.State.StartedAt}}'` in the run log so a future
   reader can tell whether the server was recycled between arms.

The harness should record the Postgres start time per run; without it, two runs
that look identical can differ in the one variable that decides the outcome.

---

# Original triage (2026-08-20), preserved

**Status at the time:** OPEN. Reproduced twice, independently, with 100%
overlap: the 6-way-concurrent full-suite run and a fully isolated 1-shard rerun
both crash the exact same 7 classes on both G1 and ZGC, zero on Generational.

## The 7 classes, identical on both runs

```
org.hibernate.orm.test.function.json.JsonExistsTest
org.hibernate.orm.test.function.json.JsonQueryTest
org.hibernate.orm.test.function.json.JsonTableTest
org.hibernate.orm.test.function.json.JsonValueTest
org.hibernate.orm.test.function.xml.XmlTableTest
org.hibernate.orm.test.query.hql.JsonFunctionTests
org.hibernate.orm.test.query.hql.XmlFunctionTests
```

`results.tsv` rows are `found=0 ok=0 failed=0 aborted=0 skipped=0` for all of
them. (Note for future readers: those zeros are what the harness writes when
there is **no** `@@RESULT` line at all, so they say nothing about how many tests
were discovered — they are not evidence that the crash preceded discovery.)

**Run 1** — full 4548-class suite, 3 GCs, 6-way concurrent shards, live
Postgres, `dev` HEAD `509710ba8`:

| GC | CRASH count | classes |
|---|---:|---|
| ZGC (default) | 8 | the 7 above + `schemaupdate.MySQLLobSchemaCreationTest` (one-off) |
| G1 | 7 | exactly the 7 above |
| Generational | 0 | — |

**Run 2** — same host, same HEAD, isolated rerun, 1 shard, no concurrency:

| GC | CRASH | wall |
|---|---:|---:|
| G1 | 7/31 (exactly the 7) | 83m20s |
| ZGC | 7/31 (exactly the 7) | 32m32s |

## Crash signature (`hs_err_pid3512.log`, G1 arm)

```
EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x00007FF633CD9D12
Faulting access: read at address 0x00007FF9E1B84694
gc collector: g1
jit: faulting pc not attributed to a compiled method
```

Captured Java frames were all JUnit Platform / Jupiter engine bootstrapping. The
faulting PC was not attributed to a compiled method and the native stack was
offsets-only — no symbolized stack was ever captured, which is why this page
could not be closed on its own evidence.
