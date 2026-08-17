# `SearchDomainTest` / `DnsNameResolverTest` — regressed from documented-FIXED state

**Status: CLOSED 2026-08-17** — root cause and fix in
`fixed-suite-bugs/netty/dns-localhost-bind-family-and-datagramsocket-doors-FIXED-20260817.md`.
`SearchDomainTest` is 7/7 and `DnsNameResolverTest` 224/232 (HotSpot's exact
score) on Windows.

This page was right that the regression was real and Windows-only, and right to
hold that line against a green Linux run. Its live hypothesis — the
`#[cfg(target_os = "linux")]` epoll refresh in
`native_dc_bind`/`selector_register` having no Windows equivalent — was wrong,
and a Windows arm for that (`nudge_blocked_poll`) already existed. The actual
mechanism is a **resolver disagreement**: `dc_socket_addr` handed the OS the
hostname `"localhost"` instead of the address the JVM had already resolved, and
Windows `getaddrinfo` orders `::1` first where glibc orders `127.0.0.1` first.
That is the whole platform split this page recorded, and it has nothing to do
with selectors.

Kept for the platform-split discipline it demonstrates, not for its hypothesis.

**Historical status below:** OPEN, REGRESSION (2026-08-13). Found on Windows rerunning netty's
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

---

## Re-measured on Linux, 2026-08-13 — does NOT reproduce under any collector

Added by the session that wrote the fix this page checks, on
`azureuser@20.80.105.49`, isolated (one class per process), at dev
`92d679ba1` — which contains `ae2e1d9c8`:

| collector | `SearchDomainTest` | `DnsAddressResolverGroupTest` | `DnsNameResolverTest` |
|---|---|---|---|
| default | ok=7 failed=0 (6 s) | ok=2 failed=0 | ok=196 failed=20 aborted=16 (86 s) |
| `-XX:+UseZGC` | ok=7 failed=0 (5 s) | ok=2 failed=0 | ok=196 failed=20 aborted=16 (83 s) |
| `-XX:+UseG1GC` | ok=7 failed=0 (5 s) | ok=2 failed=0 | ok=196 failed=20 aborted=16 (93 s) |

Every row matches the fix doc's claim, including the ZGC configuration this
page ran. `DnsNameResolverTest` completes in ~85 s where this page records no
result at 400 s, and `SearchDomainTest` is 7/7 where this page records 1/7.

**So the disagreement is real and is NOT about the source.** `eba7bffaa` (the
fd-identity fix) *is* an ancestor of `ae2e1d9c8` — verified with
`git merge-base --is-ancestor` — so both runs were nominally on the same code.
Two things can still differ, and they are worth checking in this order:

1. **The BINARY, not the checkout.** A CratonVM build carries the source as of
   the moment it STARTED, and a fresh mtime does not prove otherwise. The fix
   landed at 10:35 UTC; a binary whose build began before that, in a checkout
   later fast-forwarded to `ae2e1d9c8`, reproduces this page's numbers exactly
   — they are the fix doc's own "before" column, unchanged. Check the build's
   start time against `git log -1 --format=%ci eba7bffaa`, not the file mtime.
2. **A genuinely Windows-only path.** If the binary is confirmed to postdate
   the fix, this is the place to look rather than at the DNS classes:
   `native_dc_bind` now rebinds under the SAME fd id and calls
   `nio_selector::selector_refresh_udp`, which re-registers the channel so the
   selector polls the newly bound socket. `selector_register`'s epoll
   synchronisation is `#[cfg(target_os = "linux")]`; Windows takes a different
   arm. A refresh that does not take effect there would present exactly as
   this page describes — bind succeeds, sends work, and inbound datagrams never
   arrive, so `SearchDomainTest` falls back to its pre-fix count and
   `DnsNameResolverTest` waits out its resolver timeouts.

The minimal discriminator for (2) needs no netty at all — it is the ~50-line
repro from the fix doc (two `NioDatagramChannel`s on loopback, one 4-byte
datagram). `received=false` on Windows with a fix-carrying binary confirms it
and localises it to the selector refresh; `received=true` points back at (1).

Leaving this page OPEN: "does not reproduce on Linux" is not "is not real", and
whichever of the two it turns out to be is worth recording here.
