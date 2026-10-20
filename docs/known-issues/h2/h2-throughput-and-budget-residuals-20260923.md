# H2 — the census classes that still miss, or only just make, the 300 s cap or an in-test time budget (OPEN 2026-09-23)

| | |
|---|---|
| **Status** | OPEN. No defect is known behind any of these four. `TestTransaction` fails an in-test 50 ms budget, though not every time: the 2026-09-23 full-suite run passed it in 16 s. `TestFileSystem` does not finish even in 2,400 s. `TestBenchmark` passes in 2,245 s. `TestBtreeIndex` passes in 81–192 s, so the 300 s cap is a matter of host load. |
| **Source** | The 2026-09-22 class-by-class census, resolved in `nonpassed-classbyclass-census-RESOLVED-20260923.md`. That page fixed seven defects, and 20 of its 22 new classes now pass. These are the remaining two, plus the two that pass only just, or only given more time. |
| **Measured on** | linux-x64 (Azure 8-core, shared), Temurin 25.0.4, `--jdk-only`, default collector, `--Xmx 1g`. Everything is against HotSpot on the same host. |

## `org.h2.test.db.TestTransaction` — a 50 ms lock budget

`TestAll` sets `lockTimeout = 50` ms. In `testMergeUsing`, two sessions MERGE
the same 50 rows, and the second waits for the first to commit. It passes only
if the first session's whole 50-row batch plus commit fits inside 50 ms.

| `MergeProbe` (the test's own SQL) | batch + commit | inside 50 ms |
|---|---|---|
| HotSpot | 5–26 ms | 6/6 |
| CratonVM | 70–300 ms | 2/6 |
| CratonVM, `LOCK_TIMEOUT=2000` | 210–580 ms | 4/4 |

With the timeout raised, every run commits and the waiter proceeds, so this is
not a lost wakeup. Because the budget is a race against wall-clock time, the
class passes when a run happens to fit inside it. It did in the 2026-09-23
full-suite run, in 16 s. The test has the same shape as `TestBnf`/`TestWeb`'s 100 ms
`Sentence` budget (`not-bug-h2-bnf-ruleelement-link-null-npe-autocomplete.md`).
It closes when a single-row MERGE on this VM costs under about 1 ms. Today it
costs 1.4–6 ms.

## `org.h2.test.unit.TestFileSystem` — direct-buffer bytes, one native crossing each

HotSpot runs the whole class in 6.6 s. On this VM it spends its time in
`testConcurrent` over the `nioMemLZF:` filesystems. That code compresses and
expands 64 KB pages through **direct** `ByteBuffer`s, one `get()`/`put()` per
byte.

Under `--jdk-only`, `ByteBuffer.allocateDirect` is real bytecode, so its
memory comes from `Unsafe.allocateMemory`: an arena **handle** (bit 62 set, see
`unsafe_natives_ext::unsafe_arena::ARENA_TAG`), not an OS pointer. Every byte
access is then a JIT → `Unsafe.getByte/putByte` native crossing plus an
`ArenaStore::locate` range lookup. `perf` on the probe below is 38 % native
dispatch (`safe_native_call_impl`, `forward_jit_reference_args`,
`try_jit_site_cached_native_dispatch`, `decode_dispatch_values_into_shaped`, `jit_invoke_virtual_mic_body`, `forward_jit_arg_at`)
and 12 % arena (`unsafe_arena_copy_out`/`_in`, `ArenaStore::locate`).

| `NioMemProbe`, 10,000 write+read ops | HotSpot | CratonVM |
|---|---:|---:|
| `memFS:` | 59 ms | 534 ms |
| `memLZF:` | 101 ms | 601 ms |
| `nioMemFS:` | 46 ms | 297 ms |
| `nioMemLZF:1:` | 773 ms | > 100 s |

The first three are ordinary 5–10× throughput. The fourth is this wall, and
the class cannot finish in 300 s while it stands.

This wall was already known before the census, measured per filesystem prefix
on 2026-08-07: `nioMemLZF:1:` 995 s against HotSpot 0.8 s, and
`encrypt:0007:` / `cache:encrypt:0007:` 261 s / 207 s against 1.3 s / 0.9 s.
The analysis and the two bounded projects that would close it are in the block
comment above `DbbElemFields` in `native-io/src/direct_buffer.rs`:

* stop bailing to bytecode on an out-of-range index, so the element natives
  can claim LEAF;
* a JIT intrinsic that lowers the element access inline. That cannot be
  emitted while `allocateDirect` memory is an arena handle, because inline
  code cannot do the locked map probe that resolves one.

The census's own FAIL on this class was a different, fixed defect (the shared
`Util$BufferCache`, D2 there). With it gone, the class runs into this wall
instead. The netty compression wall
(`compression-cluster-testhugedecompress-180s-throughput-wall-20260827.md`) has
the same one-crossing-per-element shape on heap buffers.

Given 2,400 s instead of 300 s, the class still did not finish (2026-09-23, with
every census fix in).

## `org.h2.test.synth.TestBtreeIndex` — flat throughput

HotSpot 2.5 s. `main` runs the test three times with `config.big`, which is
12 iterations of `testAddDelete` (999 ordered scans over a shrinking
1,000-row table, 500,499 rows read) plus a 1,000-insert string-key fuzz.
`BtreeProbe` isolates `testAddDelete`: 8 s per iteration on CratonVM against
0.15–0.6 s on HotSpot. `perf` is flat (interpreter dispatch, JIT helper calls,
heap-address checks, allocation) with no discrete cliff. That is general
execution speed, as `hangs-true-vs-perfcliff-RESOLVED-20260821.md` already
concluded for this class.

Part of the time was a discrete cliff, and it is gone. Each duplicate-key
insert in the fuzz builds an error message that quotes a key of up to 8,000
characters through H2's `StringUtils.quoteStringSQL`, and that was quadratic
until `String.codePointAt` stopped decoding the whole string per call (D7 in
the census page). Whole class, on the same loaded shared host: 574 s without
that fix and **192 s with it, PASS**. HotSpot takes 2.5 s. The rest is the flat
gap above, so at the 300 s cap it now passes or times out depending on host
load. It is still here for that margin, not for a defect. The 2026-09-23
full-suite run passed it in 81 s.

## `org.h2.test.store.TestBenchmark` — HotSpot needs 158 s of the 300 s cap

With a 2,400 s cap it **passes, in 2,245 s**, against HotSpot's 158 s (14×, on
the loaded shared host). There is no OOM, so the 2026-08-18 write-buffer fix
(`bug-h2-testbenchmark-writebuffer-oom-at-1g-FIXED-20260818.md`) holds. The
census HANG is the 300 s cap against a workload that takes HotSpot itself more
than half of it. That is pure throughput, and nothing short of a 2× faster VM
fits it under 300 s.

In the 2026-09-23 full-suite run, the class instead crashed at 192 s. That was
a SIGSEGV in the GC root snapshot (`deposit_root_snapshot_inner`), and `dev`
itself does the same on several H2 classes (see
`nonpassed-classbyclass-census-RESOLVED-20260923.md`). It is not this page's
throughput gap.

One observation for whoever takes this on: the same run's
descriptor-coercion census reported 2,235,797 `primitive-into-reference` reads,
all on one slot (`class_id=12 index=0`). They do not change the test's result,
but a count that size is worth resolving with `CRATONVM_DBG_LAYOUT=1` before
profiling.

## Re-measuring

`tools/probes/MergeProbe.java`, `NioMemProbe.java` and `BtreeProbe.java`. Each
file's header gives its arguments, and each needs H2's `target/classes` on the
classpath. Run them on HotSpot and CratonVM with the same classpath. To run a
class alone:

```bash
cratonvm --java-home <jdk-25> --Xmx 1g -c <h2 classes:test-classes:deps> org.h2.test.unit.TestFileSystem
```
