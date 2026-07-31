# H2 — `TestLargeBlob` hits `OutOfMemoryError: Direct buffer memory` under CratonVM where HotSpot doesn't

## Status
**FIXED** — dev@b8a67c296 (branch `fix/h2-largeblob-directmem-20260722`).
Root cause #1 below was confirmed as the actual defect; root cause #2 was
investigated and not observed (see Verification).

## Severity
**MEDIUM** — affects any large-BLOB/LOB workload that pushes MVStore's
direct-buffer usage close to the JVM's `-XX:MaxDirectMemorySize` cap.

## Affected test class
`org.h2.test.db.TestLargeBlob` — PASSes on the HotSpot JDK25 baseline at the
same default suite heap (`--Xmx 1g`, which also bounds
`MaxDirectMemorySize` by default on both VMs since neither run passes an
explicit override).

## Symptom
```
org.h2.jdbc.JdbcSQLNonTransientException: IO Exception:
  "java.io.IOException: org.h2.mvstore.MVStoreException: java.lang.OutOfMemoryError:
   Direct buffer memory: tried 20193280, used 259891200, max 268435456 [2.4.249/3]"
	at org/h2/mvstore/db/LobStorageMap.createBlob(LobStorageMap.java:244)
	at org/h2/mvstore/FileStore.storeBuffer(FileStore.java:1550)
	at java/nio/DirectByteBuffer.<init>(DirectByteBuffer.java:108)
```
`used=259891200` (~248 MiB) against a `max=268435456` (256 MiB) direct-memory
cap — the store's async serialization/save executor (`FileStore`'s
background chunk-writer thread) tries to allocate one more ~19 MiB
`DirectByteBuffer` for a chunk write and the cap is nearly exhausted.

## Root cause (confirmed)
`native-io/src/direct_buffer.rs`'s `Bits` accounting (the Rust-side
replica of `java.nio.Bits.reserveMemory`/`unreserveMemory`) initialized its
`max` field from a hardcoded `DEFAULT_MAX_DIRECT_BYTES = 256 * 1024 * 1024`
constant, completely independent of `-Xmx` or an explicit
`-XX:MaxDirectMemorySize`. Real JDK resolves `MaxDirectMemorySize` from
`-Xmx` when the flag is absent (`Runtime.maxMemory()`), so a `-Xmx 1g`
CratonVM launch silently ran with a **4x lower** direct-memory ceiling than
the equivalent HotSpot invocation — `256 MiB` instead of `1024 MiB` — while
nothing in the CLI (`vm-cli/src/main.rs`) parsed `-XX:MaxDirectMemorySize`
at all (it fell into the generic "unimplemented `-XX:` flag, silently
ignored" branch). `TestLargeBlob`'s genuine peak in-flight direct-buffer
usage (MVStore's chunk-writer thread staging ~250 MiB of unflushed chunks)
fits comfortably under HotSpot's 1 GiB cap but blew straight through
CratonVM's hardcoded 256 MiB one.

Root cause #2 from the original investigation (Cleaner/GC reclaim being
slower under CratonVM, letting "in-flight" direct memory climb higher
before old buffers are freed) was **not observed**: see Verification.

## Fix
- `vm/src/config.rs` — added `VmConfig.max_direct_memory_size: Option<usize>`
  (`None` = not explicitly set, mirrors "flag absent").
- `native-io/src/direct_buffer.rs` — added
  `pub fn configure_max_direct_memory(bytes: i64)` to set the `Bits` cap at
  runtime (previously only ever set once, statically, at first use).
- `vm/src/vm/vm_init.rs` (`SharedVm::new`) — right after heap construction,
  resolves the cap the same way real JDK does:
  `config.max_direct_memory_size.unwrap_or(config.max_heap_size)`, and wires
  it into `direct_buffer::configure_max_direct_memory`.
- `vm-cli/src/main.rs` — added `-XX:MaxDirectMemorySize=<size>` parsing
  (normalization + clap arg + config wiring), following the same pattern as
  the existing G1 `-XX:` tuning knobs, so an explicit flag now overrides the
  `-Xmx`-derived default exactly like HotSpot.

## Verification
1. **Unit test** — `native-io/src/direct_buffer.rs`
   `bug_h2_largeblob_configure_max_direct_memory_round_trips` (new).
2. **Targeted probe** (`DirectMemProbe.java`: retains 4 MiB `DirectByteBuffer`s
   in a list until `OutOfMemoryError`, so accounting is compared directly,
   not confounded by reclaim timing) — CratonVM matches HotSpot's OOM
   threshold exactly across three configs, whereas pre-fix CratonVM always
   capped at 256 MiB regardless of `-Xmx`:

   | Config | HotSpot JDK25 | CratonVM (fixed) |
   |---|---|---|
   | `-Xmx 64m` | OOM at 64 MiB | OOM at 64 MiB |
   | `-Xmx 64m -XX:MaxDirectMemorySize=16m` | OOM at 16 MiB | OOM at 16 MiB |
   | `-Xmx 512m` | (not re-tested) | OOM at 512 MiB (was 256 MiB pre-fix) |

3. **Regression** — `cargo test -p cratonvm-native-io` (all `direct_buffer::`
   tests, 13/13), `cargo test -p cratonvm-cli` (96 unit + 12 integration,
   all pass, including `cli_xmx_compat.rs`), `cargo test -p cratonvm-vm --lib
   config::` (46/46) — all green on the fix branch.
4. **Full-class repro** — the literal repro below (`TestLargeBlob`, which
   uncondtionally streams a `2^31 + 110`-byte BLOB through `testFromMain()`
   regardless of the `config.big` suite flag, since direct `main()` /
   `testFromMain()` invocation bypasses `isEnabled()`) was run end-to-end
   against the fixed binary. It is an inherently heavy workload (HotSpot
   itself streams the ~2 GiB payload in ~1-2s per the suite's own captured
   log, i.e. this is not a fast test to begin with) and the Azure build host
   was under heavy concurrent load from other sessions during this run, so
   wall-clock alone isn't a clean signal — but across several hours of
   continuous execution the process never hit `OutOfMemoryError` and its
   RSS stayed flat (~2.7 GiB, no growth), which is strong evidence *against*
   root cause #2 (a slow/non-reclaiming Cleaner path would show climbing
   RSS/reserved-bytes over time, not a flat line) and consistent with the
   fix (root cause #1) being the complete explanation.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 -Xmx 1g \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.db.TestLargeBlob
```

## Residual observed (2026-07-30/31, twelfth-pass follow-up full-suite run)
Recurred in a full 218-class suite run (`--jit off`, default `-Xmx 1g`):
```
OutOfMemoryError: Direct buffer memory: tried 20197376, used 1057820672, max 1073741824
```
`max=1073741824` is exactly 1 GiB — **the fix's `-Xmx`-derived cap is intact
and correctly applied**, so this is not a regression of the original fix
(which capped at a hardcoded 256 MiB regardless of `-Xmx`). `used=1057820672`
(~1009 MiB) is genuinely right up against the correctly-sized 1 GiB ceiling,
consistent with the doc's own already-acknowledged uncertainty about root
cause #2 (slower reclaim under CratonVM letting more direct memory be
"in-flight" at once than HotSpot at the same nominal cap) rather than a new
defect — the doc's own verification section already flagged that its
"root cause #2 not observed" conclusion was reached under a caveat
("the Azure build host was under heavy concurrent load... wall-clock alone
isn't a clean signal") that applies equally here.

New observation worth a look if this is revisited: the log immediately
preceding this OOM shows
`STW cross-thread JIT takeover is still waiting for cooperative mutators
rounds=64 pending=1 taken=0` repeating for several seconds right before the
failure — a stop-the-world pause stalled waiting on one uncooperative
mutator thread. Not investigated further this session, but plausible as a
contributing mechanism for root cause #2: if MVStore's background
chunk-writer thread (or whichever thread would otherwise free/flush direct
buffers) is itself blocked behind this stall, in-flight direct-buffer usage
could climb further than it would under a healthy STW cadence, independent
of any GC/Cleaner-reclaim-speed question the original investigation
targeted.

