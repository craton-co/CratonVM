# `CertificateBuilderTest` "FAIL on all 3 collectors" is the documented FIXED state, not a reopening

**Status: NOT A BUG — closing the question, no doc needed in the OPEN sense.**
Written 2026-08-16/17, commit `3ef3eb744`, Windows host, `cratonvm.exe`
release build, to head off future sessions re-investigating this as a
reopening of
`fixed-suite-bugs/netty/pkitesting-pqc-and-initverify-FIXED-20260813.md`.

Same shape as this session's sibling finding,
`pemencodedtest-aborted-status-not-a-regression-20260816.md` — a different
status label, same underlying non-issue.

## Why this looked like a regression

A same-day full 657-class 3-collector parallel suite run recorded
`CertificateBuilderTest` with class-level status `FAIL` on all three
collectors (generational, G1, ZGC), and it was assigned in this session as a
suspected CratonVM regression alongside two genuinely-broken DNS classes (see
`dnsnameresolvertest-searchdomaintest-hang-fail-20260816.md`).

## Isolation result (G1, isolated)

```bash
cd apps/netty-suite-runner
printf 'io.netty.pkitesting.CertificateBuilderTest\n' > /tmp/certgroup.txt
./run-netty-suite.sh --list /tmp/certgroup.txt --gc g1 --shards 1 --out runs/repro
./run-netty-suite.sh --list /tmp/certgroup.txt --hotspot --shards 1 --out runs/repro
```

| arm | found | ok | failed | aborted |
|---|---|---|---|---|
| CratonVM G1 (isolated) | 74 | **39** | 28 | 7 |
| HotSpot 25 (isolated) | 74 | **39** | 28 | 7 |

Identical counts, and the per-test diff is exact: extracting every
`@@TESTFAIL ... CertificateBuilderTest <method>` display name from both raw
logs gives **19 unique failing/aborted test identifiers on each side, and
`comm -13`/`comm -23` between the two sorted lists produce empty output in
both directions** — no test fails on CratonVM that passes on HotSpot, and none
the other way. This is exactly the byte-for-byte match the 2026-08-13 FIXED
doc recorded (`found=74 started=74 ok=39 failed=28 aborted=7` on both VMs, and
"the per-test failing sets are identical in both directions").

## The actual explanation

The suite runner's per-class status field reports `FAIL` whenever a class has
`failed > 0`, regardless of whether that failure count matches HotSpot's own.
74 tests include 35 that HotSpot itself fails or aborts (SLH-DSA unavailable
on JDK 25, `BCJSSE`, `rsa4096`/`rsa8192` aborts — all documented in the FIXED
page as expected, HotSpot-side red), so `CertificateBuilderTest` will *always*
show class status `FAIL` on any VM that matches HotSpot's behavior here,
including HotSpot's own run of itself in this session's cross-check. Today's
full-suite run recorded exactly the steady-state "39 ok / 28 failed / 7
aborted" result the 2026-08-13 fix produced, reported through a status label
(`FAIL`) that reads alarmingly at a glance but carries no information beyond
"this class has at least one non-passing test" — which was never claimed to
be zero.

## Related

- `fixed-suite-bugs/netty/pkitesting-pqc-and-initverify-FIXED-20260813.md`
  — the fix this doc confirms is still holding, exactly.
- `fixed-suite-bugs/netty/keypairgenerator-getinstance-accepts-any-algorithm-FIXED-20260813.md`
  and `fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`
  — the other two records read for this triage; neither's described symptoms
  (lenient `getInstance`, the `X509CertImpl.getAlgorithm()` receiver
  confusion, the Inet6/SHA-1-OID causes) reappear here — today's failing set
  is exactly HotSpot's own, not a new symptom.
- `pemencodedtest-aborted-status-not-a-regression-20260816.md` — the same
  "status label counts a non-zero bucket, not a regression" shape, one status
  value over (`ABORTED` there, `FAIL` here).
- `dnsnameresolvertest-searchdomaintest-hang-fail-20260816.md` — the other two
  classes assigned in the same session pass; unlike this one, those two *are*
  real, CratonVM-specific regressions confirmed against a clean HotSpot run.
