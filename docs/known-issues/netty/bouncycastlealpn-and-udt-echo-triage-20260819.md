# `BouncyCastleEngineAlpnTest` (not a bug) and `NioUdtByteRendezvousChannelTest` (different failure mode than HotSpot — worth a closer look)

**Status: 1 closed, 1 flagged open.** Investigated 2026-08-19 (Azure host,
dev `b4d79475c`). Split out of `fail-hang-crash-rerun-20260817.md`'s "misc
singles" entry.

## `BouncyCastleEngineAlpnTest` — NOT a CratonVM bug

```
java.lang.ClassNotFoundException: org.bouncycastle.jsse.provider.SSLContext.TLSv1_3
    at io.netty.handler.ssl.SslUtils.getSSLContext(SslUtils.java:255)
```

HotSpot: `found=1 started=1 ok=0 failed=1` — fails identically. BouncyCastle's
JSSE provider isn't resolvable from this environment's classpath for either
VM. Not investigated further (a classpath/dependency-resolution question,
not a VM one).

## `NioUdtByteRendezvousChannelTest` — both VMs fail `basicEcho()`, but differently

CratonVM: `TimeoutException: basicEcho() timed out after 10000 milliseconds`
— the test never completes within its bound.

HotSpot: `AssertionFailedError: expected: <1968128> but was: <1906688>` —
the test *completes* but the echoed byte count is short by 61,440 bytes.

Both fail (`found=2 failed=1` on both), so a shallow read says "not
CratonVM-specific" — but the failure *shapes* are different enough that
this deserves a real look rather than being closed on that basis alone.
HotSpot's failure looks like a known-flaky characteristic of netty's UDT
transport itself (a byte-count short-read, independent of which VM runs
it — UDT is a UDP-based unreliable-by-construction transport netty layers
reliability onto, and this specific test is exactly the kind of thing that
flakes under load). CratonVM's failure — a hard 10-second timeout with no
data exchanged at all — could be the same underlying UDT flakiness landing
worse, or could be a genuine CratonVM gap in UDT socket/native handling
that prevents the exchange from progressing at all. Not distinguished here.

## What's needed before closing this

- Repeat both arms several times (UDT transport tests are inherently
  load-sensitive; barchart-udt is a native library with its own timing
  characteristics) to see whether HotSpot's short-read is consistent or
  itself flaky, and whether CratonVM ever completes instead of timing out.
- If CratonVM never completes even on a quiet host, look at what
  `basicEcho()` actually blocks on (a stack dump via
  `--stack-dump-on-timeout`, same technique used in the quarkus
  `QuarkusTestProfileAwareClassOrderer` investigation, would show whether
  it's a genuine hang or just very slow).

## Repro

```bash
cd apps/netty-suite-runner
java @common.args -Dcraton.batch=1 CratonRunner io.netty.test.udt.nio.NioUdtByteRendezvousChannelTest
# swap java for cratonvm-netty-fhc20260817-{default,g1,zgc} @common.args -XX:+Use{G1,Z}GC
```

## Related

- `fail-hang-crash-rerun-20260817.md` — where these were first flagged as
  untriaged singles.
