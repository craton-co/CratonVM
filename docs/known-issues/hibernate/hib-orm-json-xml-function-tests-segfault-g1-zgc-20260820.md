# hibernate-orm JSON/XML function tests SIGSEGV under G1/ZGC — the faulting instruction is decoded: a jump table indexed by a corrupt `Value` discriminant

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

## 2.4 What is left for whoever reopens this

1. The producer (§2.2) — the stale-receiver defect that puts two heap pointers
   in a `Value` cell. That is the actual bug; everything above is containment.
2. If a crash with this signature recurs, it should now be *rarer and
   better-labelled*: the census counts corrupt cells and the guard records the
   slot address and both raw words. §0.5 item 1 still stands and is still the
   single most valuable thing to do — **keep the binary next to the `hs_err`**.
3. §0.5 item 2's audit question (can a G1/ZGC reader walk past an object's real
   slot count?) is answered for the concurrent marker in **§2.5** — yes, by
   TOCTOU on the header, now fixed — and remains open for
   `g1::for_each_flat_object_reference`. `0x5B == 91` from the register dump remains unexplained.

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
