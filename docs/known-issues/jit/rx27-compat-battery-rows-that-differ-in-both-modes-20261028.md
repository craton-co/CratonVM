# Facts-battery rows that differ from HotSpot in both modes (left after the `--compatible` fixes)

Status: OPEN (triage only; not `--compatible` defects)
Area: probes and the JIT / runtime areas named per row
Found: rx27 wave 3, lane compat, 2026-10-28. Binary `cvm-compat-c2`; batteries `cmp2` (`--compatible`), `cmp2f`
(`--compatible`, `CRATONVM_COMPAT_FAST_BOOT_JIT=0`), base `b3j` (`--jdk-only`), HotSpot 25 runs `hs` and `hs2`.

After the compat fixes 83 of 89 probes equal HotSpot in `--compatible` (both boot arms). The other 6 give the same
lines as `--jdk-only` (`b3j`), so none of them is a `--compatible` defect:

| probe | difference | class |
|---|---|---|
| `J27DeoptRows` | `exception-after-deopt-repeat-mismatches`: HotSpot 4462 (`hs`) / 25 (`hs2`), CratonVM 0 | HotSpot's own count varies run to run (`OmitStackTraceInFastThrow`); the row is not deterministic |
| `J27RematRows` | `warm` checksum differs between `hs` and `hs2` too | nondeterministic row |
| `J27MiscRows` | `null-linkedlist-addall bad=599/879` and `-at ... null` on HotSpot | HotSpot fast-throw (message-less preallocated NPE after C2); CratonVM always gives the helpful message |
| `UnrollDeoptRows` | HotSpot prints `through NPE null` / `through AIOOBE null` | same fast-throw behaviour, deterministic on HotSpot |
| `J27Mh2Rows` | ICCE `... disagree on InnerClasses attribute` top frame: HotSpot `Class.getDeclaringClass0`, CratonVM `Class.isTopLevelClass` | reflection native frame shape (both modes) |
| `R23SegMono` | only identity-hash text in `MemorySegment` messages (`heapBase: [B@28a418` vs `[B@77`) | equal to HotSpot once `@<hash>` is normalised |

## What would close it

* The probes: make `J27DeoptRows` / `J27RematRows` / `J27MiscRows` print deterministic facts (or the battery's
  compare normalise them), and `R23SegMono` hide identity hashes.
* `UnrollDeoptRows` / `J27MiscRows`: HotSpot's `-XX:+OmitStackTraceInFastThrow` (default) hands out preallocated,
  message-less exceptions from hot C2 code; matching it is a JIT/runtime decision (see existing fast-throw pages,
  `CRATONVM_DBG_FAST_THROW_HANDOUT`).
* `J27Mh2Rows`: the frame that raises the `InnerClasses` ICCE should be `getDeclaringClass0`, as on HotSpot.
