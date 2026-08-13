# `SearchDomainTest` / `DnsNameResolverTest` — regressed from documented-FIXED state

**Status:** OPEN, REGRESSION (2026-08-13). Found on Windows rerunning netty's
non-passed list against commit `ae2e1d9c8` (`-XX:+UseZGC`), isolated
(`--shards 1`, no contention) so this is not a shard-flakiness artifact.

## The claim on record

`docs/internal/fixed-suite-bugs/netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md`
states, dated 2026-08-13, after the batch-09 `NioDatagramChannel.bind()` fix
landed:

| class | claimed after-fix state |
|---|---|
| `io.netty.resolver.dns.SearchDomainTest` | **ok=7 failed=0** (4 s) |
| `io.netty.resolver.dns.DnsNameResolverTest` | **ok=195 failed=21 aborted=16, 66 s** (no longer a hang) |
| `io.netty.resolver.dns.DnsAddressResolverGroupTest` | ok=2 failed=0 |

`docs/internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`
repeats the same claim in its own summary table (cause 3: "the hang is gone").

## What actually happens now, isolated, on `ae2e1d9c8`

```
class                                          status  found  ok  failed  ms
io.netty.resolver.dns.SearchDomainTest         FAIL    7      1   6       4805
io.netty.resolver.dns.DnsNameResolverTest      HANG    0      0   0       (rc=124, 400s timeout — did not complete)
io.netty.resolver.dns.DnsAddressResolverGroupTest  PASS  2    2   0       4475
```

`SearchDomainTest` is back to **1/7** — the exact pre-fix number the internal
doc's own "before" column records. `DnsNameResolverTest` no longer completes
at all even given **400s** (well over the 66s the fix doc measured, and over
the suite's normal 180s cap too) — worse than "still has 21 failures of its
own," it doesn't finish. `DnsAddressResolverGroupTest` is the one class of
the three still matching the documented fix.

Run individually (`--shards 1`, `--timeout 400`), not under shard contention
— ruling out the kind of load-dependent flakiness seen elsewhere in this
session (e.g. hibernate-reactive's `Multithreaded*` tests). This is
reproducing cleanly, not intermittently.

## Not yet done

- Bisecting which commit between the fix landing and `ae2e1d9c8` reintroduced
  this — a lot of unrelated netty/JIT/native work landed on `dev` in between
  (see this doc's neighbors in `docs/known-issues/netty/` and the closed
  batches in `investigate-INDEX.md`), so this needs a real bisect, not a
  guess.
- HotSpot cross-check on this exact commit/classpath wasn't rerun here (the
  internal doc's own HotSpot baseline is presumably still valid and
  unaffected, since nothing on the HotSpot side changed) — worth a quick
  confirmation pass before deep-diving CratonVM.

## Repro

```bash
cd apps/netty-suite-runner
printf '%s\n' io.netty.resolver.dns.SearchDomainTest io.netty.resolver.dns.DnsNameResolverTest io.netty.resolver.dns.DnsAddressResolverGroupTest > /tmp/dns.txt
CV_BIN=bin/cratonvm-netty-zgc.exe bash run-netty-suite.sh --list /tmp/dns.txt --gc zgc --shards 1 --timeout 400 --out /tmp/repro
```

## Related

- `docs/internal/fixed-suite-bugs/netty-pcap-write-handler-udp-bind-and-tcp-close-FIXED-20260813.md`
  — the fix this regressed from (§1c specifically, the `NioDatagramChannel`
  fd-id/selector-registration fix).
- `docs/internal/fixed-suite-bugs/netty-batch11-inet6-and-sha1-oid-CLOSED-20260812.md`
  — repeats the same now-incorrect "SearchDomainTest 7/7" claim in its
  summary table; both docs need a correction or an explicit
  "regressed since" note once this is bisected.
