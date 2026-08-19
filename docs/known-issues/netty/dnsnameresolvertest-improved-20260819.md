# `DnsNameResolverTest` — no longer hanging, 216/232 pass, residual is IPv6-assumption aborts

**Status: update, likely improved since the last characterization.**
Measured 2026-08-19 (Azure host, dev `b4d79475c`, isolated `--shards 1`
rerun). The earlier `dnsnameresolvertest-searchdomaintest-hang-fail-20260816.md`
doc (characterizing a `HANG` caused by DNS queries routing to the
non-routable address `0.0.0.1` instead of loopback) is no longer present in
`docs/known-issues/netty/` at the time of this rerun — either fixed and
retired by the campaign, or lost to the same concurrent-session file-loss
this campaign has hit before. This note exists so the improvement is
recorded regardless of what happened to the original doc.

## Current state

```
@@RESULT io.netty.resolver.dns.DnsNameResolverTest found=232 started=232 ok=216 failed=0 aborted=16 skipped=0
```

No hang, no crash — **216/232 (93%) pass**. The 16 aborts are all the same
shape:

```
org.opentest4j.TestAbortedException: assumption was not met due to:
  Expecting value to be true but was false
    at io.netty.resolver.dns.DnsNameResolverTest.testResolveAllHostNameIpv6(DnsNameResolverTest.java:982)
```

`testResolveAllHostNameIpv6`, parameterized 16 ways, each hitting an
`assumeTrue`-style IPv6-availability check. Not cross-checked against
HotSpot in this pass, but the shape (an environment-capability assumption,
not an exception during actual resolution) makes this look like ordinary
"this host doesn't have IPv6 configured" skipping rather than a defect —
consistent with every other `ABORTED`-on-assumption cluster found in this
same investigation round (`buffer-alignment-abort-cluster-not-a-cratonvm-bug-20260819.md`).

## Disposition

Not treated as an open CratonVM bug. If the `0.0.0.1`-routing defect the
retired doc described is genuinely fixed, this is a closed win worth
confirming with a HotSpot side-by-side; if it just wasn't hit by this
particular class/parameterization mix, it may still be live elsewhere.
Flagging rather than closing outright, since the original diagnostic doc
isn't available to compare against directly.

## Related

- `fail-hang-crash-rerun-20260817.md` — flagged this class's status change
  (`HANG` in the original characterization → `ABORTED` here) as worth a
  look.
- `fixed-suite-bugs/netty/dns-searchdomaintest-and-dnsnameresolvertest-regressed-20260813.md` —
  an earlier round of the same two classes' history.
