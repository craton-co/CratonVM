# `NativeImageHandlerMetadataTest` × 17 — NOT a CratonVM bug, a harness environment gap

**Status: CLOSED — not a defect, confirmed on HotSpot.** Investigated
2026-08-19 (Azure host, dev `b4d79475c`). Split out of
`fail-hang-crash-rerun-20260817.md`'s "new clusters, not yet triaged" list.

## What it is

17 classes, one per netty module (`io.netty.channel`, `io.netty.handler`,
`io.netty.handler.codec`, `io.netty.handler.codec.{dns,haproxy,http,http2,
memcache.binary,mqtt,redis,sctp,smtp,socks,stomp,xml}`,
`io.netty.handler.proxy`, `io.netty.resolver.dns`), each named
`NativeImageHandlerMetadataTest`, each with one test method
(`collectAndCompareMetadata`), fail **identically** on both CratonVM and
HotSpot:

```
org.opentest4j.AssertionFailedError: Native Image reflection metadata is
required for handlers in this project. This metadata was not found under
/data/.../apps/netty-suite-runner/src/main/resources/META-INF/native-image
/null/null/generated/handlers/reflect-config.json
```

Note the path: `META-INF/native-image/null/null/generated/handlers/...` —
two literal `null` path segments where a Maven `groupId`/`artifactId` pair
belongs.

## Confirmed: not CratonVM-specific

```bash
cd apps/netty-suite-runner
java @common.args -Dcraton.batch=1 CratonRunner io.netty.channel.NativeImageHandlerMetadataTest
```

Fails with the **exact same** `null/null` path and assertion message on
stock HotSpot 25. Same for CratonVM. Byte-identical failure on both VMs —
this cannot be a VM defect.

## Why

This test computes its expected `reflect-config.json` resource path from
build-tool metadata (Maven `groupId`/`artifactId`, normally baked into a
generated properties file or the module's own resources by the real Maven
build). This harness (`run-netty-suite.sh` / `CratonRunner`) runs test
classes directly against a hand-assembled classpath — it never runs `mvn`,
so that metadata is never populated, and the path both VMs compute
literally contains the string `null` twice. The real
`reflect-config.json` this test wants to compare against does exist
somewhere in each module's tree; the test just never finds it because the
path it constructs is wrong in this environment, not because CratonVM's
reflection or class metadata differs from HotSpot's.

## Disposition

No VM fix applies. If these 17 classes matter to future coverage numbers,
the actual fix is a harness one — either populate the Maven
`groupId`/`artifactId` metadata this test reads (however it's obtained;
not investigated further, since it's independent of VM correctness), or
exclude `NativeImageHandlerMetadataTest` from the class lists entirely as
out-of-scope for a non-Maven harness (same category as the `*IT` classes
already excluded in `partial-run-fail-hang-triage-20260817.md` for needing
a packaged-artifact metadata file).

## Related

- `fail-hang-crash-rerun-20260817.md` — where this cluster was first
  flagged as untriaged.
- `partial-run-fail-hang-triage-20260817.md` — the quarkus `*IT` classes,
  same shape of "needs real build-tool output this harness doesn't
  produce" gap.
