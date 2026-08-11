# The G1 SIGSEGV in `TestChunkedTransferEncodingWithProxy` was the diagnostic's own out-of-bounds read

| | |
|---|---|
| **Status** | **CLOSED 2026-08-11.** There is no VM defect here. The crash only ever happened with `CRATONVM_DBG=g1-dbg-reach` set, and it happened *inside* the verifier that flag enables. Fixed in `gc/src/g1.rs`, `dbg_verify_reachable_integrity`. |
| **Discovered** | 2026-08-11, in the 651-class G1 arm of the three-collector re-run that retired `g1-sigsegv-unguarded-callee-jit-frame-FIXED.md`. |
| **Reproducer** | The class alone, **`-Xmx4g`** (the suite's per-class heap for it), `-XX:+UseG1GC`, `CRATONVM_DBG=g1-dbg-reach`. ~8 min, crashed 2 of 3. |

## The verdict, in one table

Same class, same binary, same `-Xmx4g`, same `-XX:+UseG1GC`; the only variable is
whether the diagnostic flag is set.

| `CRATONVM_DBG=g1-dbg-reach` | runs | outcome |
|---|---|---|
| **on** | 3 | 2 × `EXCEPTION_ACCESS_VIOLATION` at `dbg_verify_reachable_integrity`, 1 incomplete |
| **off** | 3 | **3 × PASS** (`OK (1 test)`, 753 s / 720 s / 382 s) |

The suite run that first showed the crash had `g1-dbg-reach` on the **G1 arm
only** — I set it there to count walk breaks. The default and ZGC arms never
ran this code. So "G1-only, passes under the other two collectors", which is
what this page originally led with, was **flag-only**.

## What the verifier did

`dbg_verify_reachable_integrity` BFSes the heap from the roots after a
collection and reports any reachable slot whose target is zeroed or outside
every region. Its accept test was:

```rust
let region = self.lookup_region_for_addr(addr);
if region.is_none() || is_zeroed(addr) { /* report, don't traverse */ }
```

— i.e. *any* address landing inside some region was treated as the start of an
object. This test fills a **1 GB `byte[]` with `'A'`**
(`Arrays.fill(payload, (byte) 'A')`), so a conservative root or a stale slot
pointing into the middle of that array passes it, and the header read back
there is payload:

```
[g1][DBG-REACH] pause=0 young-serial: LIVE-REACHABLE array-elem[12]
  holder=0x25400000000 (cid=1094795585 kind=1 slots=1094795585 len=1094795585
  region=Some(442) off=0x6f000 cursor=0x0 ABOVE-CURSOR) -> 0x4141414141414141
  is WILD (region=None)
```

`cid=1094795585` is `0x41414141`. `kind=1` is Array. `len=1094795585`. The BFS
then ran

```rust
for k in 0..header.array_length() as usize {
    let raw = unsafe { std::ptr::read(data.add(k * 8) as *const u64) } as usize;
```

over 1 094 795 585 "elements", walked off the end of the arena, and took the
`EXCEPTION_ACCESS_VIOLATION` at a page-aligned address. The `bad <= 16` cap
silenced the reports long before the read ran out of mapped memory, which is
why the log shows a dozen `is WILD` lines and then a crash.

**The report contained the evidence it ignored.** `off=0x6f000 cursor=0x0
ABOVE-CURSOR` says, in the same line, that this address is past its region's
allocation cursor and therefore cannot be a live object. The BFS computed that
pair only to *print* it. The linear-walk sibling (`DBG-ZERO`, same function
family) has always bailed on exactly this condition —
`if sz < HEADER_SIZE || off + sz > cursor { break }`.

## The fix

`live_extent(addr)` returns how many bytes an object starting at `addr` may
legally occupy — `None` when the address is not in a region, or when
`off + object_total_size(header)` overruns that region's cursor. A
`HumongousStart` region is special-cased: a humongous object legitimately runs
past its start region's own buffer into the adjacent continuation regions, so
`cursor` is the wrong bound for it, and what is checked instead is that it
starts at offset 0.

Two uses:

* `check_push` refuses to traverse an address with no live extent, and reports
  it as `NOT-AN-OBJECT-START (its size does not fit its region below the
  cursor)` — a third verdict beside `WILD` and `ZEROED`;
* the element loop is clamped to `extent / 8`, so even an accepted object
  cannot be read past its own payload on the strength of an
  `array_length()` field this walk has not yet decided it can trust.

## What this does *not* change

The retirement of `g1-sigsegv-unguarded-callee-jit-frame-FIXED.md` stands, and
is strengthened: its G1 arm's crash column is now **0**, not 1, in production
configuration. Zero `[g1][WALKBRK]` across 135 evacuating processes is
unaffected — the walk-break instrument and this BFS are different code.

## The rule to take away

An instrument that is only enabled on one arm of a comparison is a **variable
of that comparison**, not a free observation. I put `g1-dbg-reach` on the G1
arm alone to measure walk breaks, and it manufactured the G1-only crash I then
spent a page attributing to G1.

Two cheap habits would have caught it immediately: run the diagnostic on every
arm or none, and symbolize the faulting frame *before* characterising the
crash — one `CRATONVM_DBG=symbolize` call named
`G1Collector::dbg_verify_reachable_integrity` and ended the investigation.

The corollary for the earlier page's advice: **`CRATONVM_DBG=g1-dbg-reach` was
not safe to recommend** on a workload with large primitive arrays until this
fix. It is now.

## Also corrected here

The original page said of `rsi=0x0000000041414141`: "nothing in this fixture
obviously writes `AAAA`, so it is more likely a partially-overwritten or
never-initialised slot than an actual payload." That was wrong in the most
direct way possible — the fixture's first statement is
`Arrays.fill(payload, (byte) 'A')`. Read the test before theorising about a
byte pattern.

It also prescribed reproducing at the suite default heap. The suite gives this
class **`-Xmx4g`** (`Resolve-ClassHeap`, `$ChunkedProxyXmx`), and at the 2 g
default the class OOMs on its own 1 GB array or flakes; three arms run at 2 g
produced only noise. The launch environment includes the per-class heap.
