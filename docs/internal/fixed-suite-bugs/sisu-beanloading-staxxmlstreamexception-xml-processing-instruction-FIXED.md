# Keycloak Sisu bean loading StAX XML processing-instruction residual — CLOSED

Status: CLOSED on current `dev` (`1520b2b8`, 2026-07-15). No VM source change was required: the original
StAX failure is not reproducible against the current runtime and retained dependency set.

## Original symptom

The original report recorded 36 Keycloak `tests/base` classes failing during `Sisu.addClassLoader()` with
`XMLStreamException: ParseError at [row,col]:[1,11]` and “The processing instruction target matching
`[xX][mM][lL]` is not allowed.” The reported run used the older `e85f76d` baseline and a now-stale Keycloak
checkout. Its retained runner no longer has a valid Keycloak checkout or compiled `KcRunner`, so it cannot
provide a current end-to-end oracle.

## Current verification

Sisu 1.6.1 was inspected directly. Its Plexus descriptor path is:

`JarURLConnection.getInputStream()` → `InputStreamReader(UTF_8)` → `BufferedReader` →
`XMLInputFactory.newDefaultFactory().createXMLStreamReader(reader)` for every
`../../../apps/META-INF/plexus/components.xml` resource visible to its class loader.

A temporary standalone probe recreated that exact path. It enumerated every JAR in the Azure host Maven cache
containing that resource, loaded the selected JARs through `URLClassLoader`, opened each descriptor by resource
URL, and consumed the full StAX stream.

- HotSpot JDK 25: `SISU_STAX_OK jars=201 resources=201`.
- CratonVM built in the isolated worktree from `1520b2b8`: `SISU_STAX_OK jars=201 resources=201`.

The release build used a dedicated target and temporary directory under
`/data/data/cratonvm-sisu-stax-xml-20260715`; it completed successfully in 8m43s. Its dedicated executable
`cratonvm-sisu-stax-xml-20260715-built` was the binary used for the CratonVM result above.

This covers the actual resource/reader/StAX sequence named in the original stack trace, including all currently
available Plexus descriptors, and establishes no observable divergence from HotSpot. The old failure is therefore
retired rather than attributed to an unproven file-I/O defect.

## Related residuals

The downstream Maven/Sisu symptom previously noted in
`../../known-issues/keycloak/welcomepagetest-stream-spliterator-zipcopy-residuals-20260715.md` is resolved by
this reclassification. That note's remaining Stream/Spliterator issue is separate and remains outside this
closure.
