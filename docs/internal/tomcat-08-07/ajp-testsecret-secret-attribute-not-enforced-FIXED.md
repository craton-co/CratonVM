# TestAbstractAjpProcessor.testSecret - AJP secret rejection preserved (FIXED)

**Status:** FIXED (2026-07-13). **Severity:** low (single focused AJP test).

## Root cause

Tomcat's `CoyoteAdapter` intentionally recycles `decodedURI` after a protocol
error and invokes `Mapper.map()` again only to identify the host for error
reporting. A recycled `MessageBytes` URI has `type == T_NULL`; Java
`Mapper.internalMap()` maps the host and returns immediately for that value.

CratonVM's native `Mapper.map()` converted the URI to a `CharChunk` before
checking the `MessageBytes` type. The empty chunk was then treated as a normal
URI and selected the root context, whose redirect replaced the AJP processor's
original 403 response. As a result, a missing AJP secret appeared as a 302.

## Fix

The native mapper now recognizes a null URI before URI conversion. It performs
only Java-equivalent host mapping (including alias/default-host fallback), sets
`MappingData.host`, and returns without context or wrapper mapping. The
protocol error status therefore remains authoritative.

## Validation

An Azure release build with the dedicated target directory
`target-ajpsecret-20260713` ran the focused `testSecret` wire-level contract
against real JDK boot classes in both interpreter (`--nojit`) and JIT modes:

```
AJP_TESTSECRET runs=1 failures=0
```

The probe preserves all three original requests: missing secret -> 403, wrong
secret -> 403, and `RIGHTSECRET` -> 200. The unrelated
[`testNoHeaders` residual](../../known-issues/tomcat/ajp-testnoheaders-response-body-not-empty.md)
remains separately tracked.
