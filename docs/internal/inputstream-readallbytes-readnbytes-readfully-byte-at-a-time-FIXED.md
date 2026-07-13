# `InputStream.readAllBytes()`/`readNBytes()` + `DataInputStream.readFully()` — one-byte-per-native-call slowness — FIXED

**Status:** FIXED. Two long-standing (present since at least May 20, unrelated to any recent
commit) native implementations looped a single-byte `invoke_virtual` call per byte instead of
bulk-reading — turning any large stream read into hundreds of thousands of individual virtual
dispatches. Manifested as an apparent "hang" in `SecurityInfoTests`/`NestedJarFileTests` (see
`securityinfo-jarsig-providers-npe-datainputstream-close-dsa-jca-chain-FIXED.md`) once those tests'
earlier blocking bugs were fixed and they could finally reach this code path.

## Symptom

After merging 139 concurrent `dev` commits into the jar-signature-verification fix branch and
rebuilding, `SecurityInfoTests`/`NestedJarFileTests` — which had passed/failed cleanly (no hang)
minutes earlier on the same code — started timing out at the suite runner's 300s ceiling with
zero output. `cratonvm.exe --stack-dump-on-timeout 30` showed the same bytecode pc sampled across
multiple 30s-apart dumps:
```
JarInputStream.checkManifest() -> InputStream.readAllBytes() -> ZipInputStream.read()I
```
This looked like a genuine deadlock (same pc every time), but isn't — see "Root cause" below for
why a byte-at-a-time loop produces an *identical* sampled pc regardless of how many iterations
have actually completed.

## Root cause

Confirmed **not a new `dev` regression** — `git log -L` on both functions shows the same
one-byte-per-call shape present since at least the May 20 "Rename rustjvm → cratonvm" commit, long
before the 139-commit wave. It was simply never exercised at this scale before: the specific test
that surfaces it (`SecurityInfoTests.getWhenJarIsSigned`, using the real `bcprov-jdk18on-1.78.1
.jar` with a ~769KB `MANIFEST.MF` and ~5700 entries) had been blocked by *earlier* bugs (see the
jar-signature-verification doc) that always threw before code ever reached this deep — so nothing
had previously run this stream-reading path against a jar anywhere near this size.

Two separate native functions in `native-io/src/lib.rs` had the same defect:

1. **`native_is_read_all_bytes`/`native_is_read_n_bytes`/`native_is_read_n_bytes_buf`**
   (`java.io.InputStream.readAllBytes()`/`readNBytes(int)`/`readNBytes(byte[],int,int)`) — looped
   `ctx.invoke_virtual(this, "read", "()I", &[])` (the **single-byte** `read()` overload) once per
   byte, collecting results into a `Vec<i32>`. For a 769KB manifest, that's ~769,000 individual
   virtual dispatches just to construct the `JarInputStream` (`JarInputStream.<init>` always calls
   `checkManifest()` → `readAllBytes()` immediately, before any signature-verification code runs
   at all). Measured: 18-70+ seconds depending on host load, vs. ~1 second after the fix.

2. **`dis_read_fully_impl`** (backing `DataInputStream.readFully(byte[])`/`readFully(byte[],int,
   int)`) — had a doc comment claiming "tries bulk read on inner stream first, falls back to
   byte-by-byte", but the actual code unconditionally looped a single-byte helper
   (`dis_read_one`, itself already correctly implemented as a *deliberately* 1-byte
   `read([BII)I` call — see its own comment on why: it must not prefetch bytes a different,
   concurrent reader of the same shared stream might need). Spring Boot loader's own
   `JarEntriesStream.assertSameContent()` calls `DataInputStream.readFully(byte[], off, len)` once
   per up-to-4KB chunk, once per **non-directory jar entry** — for the same ~5700-entry jar,
   hundreds of thousands more single-byte calls, compounding on top of bug 1.

Why a "hang" from a stack-dump's perspective: sampling a stack mid-way through hundreds of
thousands of tight-loop `invoke_virtual` calls to the *same* single-byte method always lands at
that method's bytecode entry point (pc≈0) — indistinguishable, from one or two samples, between
"stuck forever" and "making real but glacially slow progress." Only a wall-clock timing comparison
(a synthetic large-manifest jar with no signature content at all reproduced the same slowness,
ruling out anything jar-signature-specific) and, ultimately, giving the process an unbounded
timeout (it *did* eventually complete, just past any reasonable suite timeout) distinguished this
from a genuine deadlock.

## Fix

Rewrote all four functions to bulk-read via the virtual `read([BII)I` overload (16KB chunks for
`readAllBytes`/`readNBytes`, direct-into-caller's-buffer for `readNBytes(byte[],off,len)` and
`readFully`), using the existing `ctx.read_byte_array_into`/`write_byte_array_from` bulk
array-copy intrinsics (already used by other perf-sensitive callers) instead of per-element
`get_array_element`/`set_array_element` loops on top. Mirrors the pre-existing, already-correct
pattern in `native_is_transfer_to` (same file) — bulk `read([BII)I` with GC-safe `pin_native_root`/
`read_native_pin` bracketing across each call (these can trigger GC).

`dis_read_fully_impl` specifically: unlike `dis_read_one`'s deliberate 1-byte-only design (guarding
against stealing bytes from a stream shared with another concurrent reader), bulk-reading inside
`readFully(buf, off, len)` is safe — a `readFully` call is *itself* a bulk request for exactly
`len` bytes, so requesting up to the *remaining* unfulfilled portion of that same `len` per
`read([BII)I` call never reads a single byte past what the caller already asked for. Also
preserved `dis_read_one`'s documented "some streams return 0 for a non-empty request instead of
blocking" fallback (a genuine edge case, not a hang) by falling back to a scalar 1-byte read on a
0-byte bulk result, rather than misclassifying it as EOF.

## Verification

- Standalone repro (`JarVerifyRepro.java`, plain `JarInputStream` over the real bcprov jar):
  manifest construction went from 18-70+ seconds to **1.07 seconds**; full 5697-entry jar
  processing from indeterminate (never observed to complete under 400s) to **9.5 seconds total**.
- `SecurityInfoTests`/`NestedJarFileTests` via the actual suite runner: went from `HANG` (300s) to
  `FAIL` in 66.7s/89.4s — and the failures are *exactly* the pre-existing, already-documented ones
  (bug 7's nested-PKCS7 `getWhenJarIsSigned` assertion; `NestedJarFileTests`' 4 unrelated
  pre-existing failures — `getCommentAlignsWithJdkJar`, `getEntryWhenMultiReleaseEntryReturnsEntry`,
  `versionedStreamStreamsEntries`, `createOpensJar`). `NestedJarFileTests.verifySignedJar` now
  passes (previously never got a clean run to observe).
- `cargo test -p cratonvm-native-io --lib`: 348 passed, 0 failed.
- `cargo test -p cratonvm-native-builtins --lib`: 2975 passed, 11 failed — all 11 match the
  documented pre-existing baseline (6 unrelated + 5 `jboss_msc` failures, both independently
  confirmed present on a clean `dev` tip build with none of this fix's changes).

## Files changed

- `native-io/src/lib.rs` — `native_is_read_all_bytes`, `native_is_read_n_bytes`,
  `native_is_read_n_bytes_buf`, `dis_read_fully_impl` rewritten to bulk-read via `read([BII)I`.

## Note on `InflaterInputStream.close()` observation during investigation

While bisecting the slowness location via repeated stack-dump probes, one 90s-timeout run's final
watchdog dump (triggered by the harness's own process-kill, not the periodic sampler) landed inside
`InflaterInputStream.close()` and reported "1360 thread(s) dumped" before aborting. A subsequent,
longer-timeout run completed normally (62.6s) with no thread-count anomaly reported and no
different failure signature — strongly suggesting this was either a transient snapshot mid-way
through the (still, before this fix, slow) run, or an artifact of the harness's own shutdown-time
diagnostic dump rather than a genuine leak. Not investigated further since the actual reported
symptom (the "hang") was resolved and re-verified multiple times without recurrence; flagging here
in case it recurs for a future investigator.
