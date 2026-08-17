# CLOSED — `OcspClientTest` is sixteen RSA-2048 certificates, not a hang

**Status:** ✅ CLOSED 2026-08-17 on `fix/netty-sni-ocsp-rld-residuals-20260817`.
Retires the `OcspClientTest` half of
`docs/known-issues/netty/sniclienttest-ocspclienttest-triage-20260816.md`
(the `SniClientTest` half is retired in
`sniclienttest-sni-refusal-alert-FIXED-20260817.md`).

The page recorded this class as **OPEN, perf** — "never actually fails
anywhere… but at 165-169 s against a 180 s per-class timeout, against HotSpot's
6.5 s", with "not profiled yet; candidate costs to check first … are JCA object
churn and the certificate-builder path". It is now profiled. There is no
defect: the wall time is a per-operation cost, fully attributed, and the HANG
the full-suite table recorded is a timeout-budget artifact that now has an
override row.

## What it actually spends its time on

Attribution, in order of how much it explains.

**1. Sixteen RSA-2048 certificates.** The class builds
`CertificateBuilder().algorithm(rsa2048)` chains — 10 call sites, of which
`testCertIdBypass`'s three run once per parameterization, so 16 certificates
per class run. Timed directly (`CertBuildProbe`, netty's own
`io.netty.pkitesting.CertificateBuilder`, same host, same classpath):

| operation | HotSpot 25 | CratonVM | ratio |
|---|---|---|---|
| `buildSelfSigned()` rsa2048, first | 1517 ms | 21 000 ms | 14x |
| `buildSelfSigned()` rsa2048, steady | 73-193 ms | 4975-7839 ms | ~40-65x |
| `buildIssuedBy()` rsa2048 | 302 ms | 3227 ms | 11x |

16 × ~5 s is the class. Nothing else needs to be invoked to explain it.

**2. It is CPU-bound and single-threaded, not blocked.** Measured
CPU/wall = **0.89** on an 8-core box, so one core is saturated end to end. A
`--stack-sample-ms 20` profile accounts for only 6% of the run in Java frames,
which on its own reads as the stall signature — but the CPU ratio says
otherwise: the missing 94% is inside the VM's own native code (crypto, ASN.1,
limb arithmetic), which the Java stack sampler cannot see. Recording that
here because taking the sampler's coverage at face value would have sent the
next reader looking for a lock.

**3. The cost is below the certificate builder, and it is broad.** Per-primitive
(`RsaKeygenProbe`, same host):

| primitive | HotSpot 25 | CratonVM | ratio |
|---|---|---|---|
| RSA-2048 keygen (1st / 2nd / 3rd) | 144 / 79 / 39 ms | 3442 / 1798 / 720 ms | ~20x |
| `BigInteger.probablePrime(1024)` | 21 ms | 276 ms | 13x |
| `BigInteger.modPow`, 2048-bit, ×20 | 16 ms | 176 ms | 11x |
| `SHA256withRSA` sign | 4 ms | 32 ms | 8x |

Note that keygen (~0.7-1.8 s steady) is a MINORITY of the ~5 s certificate:
the remaining ~3 s is X.509 encoding + signing + BouncyCastle's own ASN.1, all
of it ordinary interpreted-bytecode cost. So this is not one hot primitive
with a fixable name; it is the whole certificate path at this VM's per-op rate.

`modPow` is already on the limb-based `BigInt` path (`phases_late.rs` wins the
registration over `math_bignum.rs`'s decimal-string one — LAST-write-wins), so
the 11x there is square-and-multiply with a Knuth-D division per step against
HotSpot's Montgomery intrinsics. That is the one named, contained lever left in
this area and it is filed separately as
`docs/known-issues/biginteger-modpow-has-no-montgomery-reduction-20260817.md`
— deliberately NOT attempted here, because it is a crypto-correctness-sensitive
rewrite and not what this page was about.

**4. One of the six methods uses the real internet.**
`simpleOcspQueryTest` opens `https://apple.com` and runs a live OCSP query. The
class total therefore swings **110-240 s run to run on the identical binary**
(four interleaved ABBA runs of two binaries measured 113.9 / 168.0 / 236.0 /
167.7 s with no correlation to which binary ran). Any factor quoted for this
class to better than ~2x is noise. The page's own "re-measure both arms
back-to-back before quoting a factor" caveat applies to itself.

## Why the suite said HANG, and what changed

Nothing was hanging. At the runner's default 180 s wall cap, a class that needs
110-240 s alone is recorded as HANG the moment the box has any other load — and
a full 657-class 3-collector parallel sweep is exactly that load. The class has
been given a timeout floor, which is what `class-overrides.tsv` exists for and
which the header there asks be added only once slow-but-legitimate has been
distinguished from a real hang. It has:

```
io.netty.handler.ssl.ocsp.OcspClientTest	600	-
```

Verified live with `./run-netty-suite.sh overrides`. 600 s is ~2.5x the worst
isolated measurement.

`apps/` is gitignored wholesale, so that row lives only in the working tree of
the main worktree — the same caveat the file's own header records.

## Results

Isolated, one class per process, `--shards 1`:

| arm | result |
|---|---|
| CratonVM G1, control | `PASS 6 found / 6 ok / 0 failed`, 113.9-167.7 s |
| CratonVM G1, this branch | `PASS 6 / 6 / 0`, 157.0-236.0 s |
| HotSpot 25 | `PASS 6 / 6 / 0`, 9-13 s |

The two CratonVM arms are indistinguishable, as expected: this branch's fixes
are a G1 allocation trigger, a G1 write-barrier memo and a `ReferenceQueue`
fast path, and this class is single-threaded certificate crypto that touches
none of them.

## Related

- `sniclienttest-sni-refusal-alert-FIXED-20260817.md` — the real defect on the
  other class the retired triage page covered.
- `resourceleakdetector-concurrentusage-is-slow-not-hung-CLOSED-20260817.md` —
  same shape of finding (a correct-but-slow class that a timeout turns into a
  HANG), retired in the same pass.
- `certificatebuildertest-fail-status-not-a-regression-20260816.md` (open) —
  the same certificate-builder cost seen from its own test class.
