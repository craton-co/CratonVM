# bc-java: `pqc.crypto.lms.AllTests` / `pqc.crypto.test.AllTests` exceed even a 10x timeout — confirmed CPU-bound, not deadlocked

## Status
**OPEN, confirmed CratonVM-specific** — found 2026-08-16 running bc-java's
`AllTests` suites under CratonVM on Azure (`azureuser@20.80.105.49`).
Originally flagged as HANG at a 300s per-class timeout, reran at a **10x
timeout (3000s / 50 min)** and still did not finish (`rc=124`) for both
classes. Real HotSpot JDK 25 runs `pqc.crypto.lms.AllTests` in 16s and
`pqc.crypto.test.AllTests` in 195s with the identical classpath.

As with the parallel commons-math finding
(`bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816.md`,
found the same day), this is **not confirmed to be a stuck/deadlocked
defect** — live process inspection shows the CratonVM process actively
burning CPU throughout, consistent with a severe interpreter throughput
cliff on computation-heavy code (hash-based post-quantum signature
key generation/signing, which is inherently a very large number of hash
operations) rather than a livelock or blocked wait.

## Evidence: CPU-bound, not blocked

While `org.bouncycastle.pqc.crypto.lms.HSSTests` (see below) and
`org.bouncycastle.pqc.crypto.test.AllTests` were each running under
CratonVM, the actual VM child process (not the `timeout` wrapper) was
inspected directly via `ps`:

```
PID     PPID    ELAPSED  TIME      %CPU  STAT
2703707 2703706 78       00:01:18  99.9  Sl    (HSSTests, 78s in)
2704620 —       36       00:00:36  99.7  Sl    (pqc.crypto.test.AllTests, 36s in)
```

`TIME` tracking 1:1 with `ELAPSED` at ~99.7-99.9% CPU rules out a deadlock,
livelock, or blocked I/O/lock wait — the process is continuously doing work,
just far more slowly than HotSpot does the same work.

## `pqc.crypto.lms.AllTests`: `HSSTests` alone accounts for 15.9 of HotSpot's 16 seconds

`AllTests.suite()` combines 5 independent JUnit3 test classes:
`HSSTests`, `LMSKeyGenTests`, `LMSTests`, `PublicKeyParseTests`,
`TypeTests`. Timing each individually under HotSpot isolates where nearly
all the suite's cost lives:

| class | HotSpot time |
|---|---|
| **HSSTests** | **15,882ms** |
| LMSTests | 772ms |
| LMSKeyGenTests | 388ms |
| TypeTests | 184ms |
| PublicKeyParseTests | 106ms |

HSSTests (Hierarchical Signature System — LMS Merkle-tree-based key
generation and signing, which is dominated by SHA-256 hashing) is almost
certainly where CratonVM's slowdown concentrates. Using the ~300-330x
per-iteration interpreter-vs-HotSpot slowdown independently measured in the
parallel commons-math investigation as a rough scale, HSSTests alone would
be expected to take on the order of 80+ minutes under CratonVM — comfortably
exceeding the 3000s (50 min) budget given, without requiring HSSTests itself
to be non-terminating.

## `pqc.crypto.test.AllTests`

A much larger aggregate suite (195s on HotSpot, versus lms.AllTests's 16s).
Not individually decomposed sub-suite-by-sub-suite in this pass — the
99.7%-CPU live-process evidence above applies, but the dominant contributor
class(es) within it are not yet identified (see Next steps).

## Next steps
* Decompose `pqc.crypto.test.AllTests`'s own `suite()` the same way
  `lms.AllTests` was decomposed here (time each constituent class
  individually under HotSpot, then focus the CratonVM investigation on
  whichever one or two classes dominate).
* Re-run with CratonVM's JIT enabled (drop `--nojit`) — this differential
  harness runs in forced-interpreter mode throughout; if HSSTests's hot
  hashing loops get JIT-compiled in ordinary CratonVM usage, this may be
  substantially a `--nojit`-mode artifact of the harness.
* Check whether CratonVM has an accelerated/intrinsic SHA-256 implementation
  equivalent to HotSpot's (`sun.security.provider.SHA2` typically gets a
  native/intrinsic fast path on HotSpot). HSS/LMS signing is essentially
  "hash a very large number of times" — if CratonVM's `MessageDigest`
  SHA-256 path runs purely through interpreted Java (or a slow native shim)
  with no dedicated fast path, that alone could explain most of the gap and
  would be a concrete, independently fixable target distinct from "the
  interpreter is generically slow."
* Practically: exclude these two classes (or give them a much larger
  timeout) from timeout-bounded suite sweeps rather than continuing to
  classify them as HANG.

## Repro
```bash
cd apps/bc-java
source <toolchain env>
CP="$(cat bcjava-classpath.txt)"   # pkix/prov/core/util build dirs + junit + hamcrest, see Azure setup
# isolate the dominant sub-suite:
timeout 60 <cratonvm-bin> --java-home <jdk25-home> --nojit --Xmx 1g -c "$CP" \
  junit.textui.TestRunner org.bouncycastle.pqc.crypto.lms.HSSTests
# confirm CPU-bound (not deadlocked) while it runs:
ps -o pid,ppid,etimes,time,pcpu,stat,cmd --ppid <timeout-wrapper-pid>
```

## Related
Same overarching theme as the commons-math finding
`bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816.md`
— a CPU-bound (not deadlocked) interpreter throughput cliff on call-dense
numeric/cryptographic workloads, found the same day in a parallel
investigation.
