# Spring Boot loader residual closure (2026-07-23)

## Scope

The 15-class `loader/spring-boot-loader` residual shard exercised nested jar
URLs, archive/launcher class loading, security metadata, nested file systems,
and large ZIP64 content.  It had both functional residuals and a whole-class
timeout in `ZipContentTests`.

## Fix

The VM now keeps the Spring Boot loader's defining-loader and URL/archive
semantics on the native paths, preserves the standard closed-file contract,
and uses bounded native ZIP/file-data fast paths for the large streamed ZIP
fixtures.  The fast paths fall back to the Java implementation for unsupported
buffer layouts and for a closed `FileDataBlock`; this retains the original
exception behavior while avoiding repeated host file reads.

## Verification

Using JDK `25.0.3.9-hotspot`, r171 of the dedicated CratonVM binary built
after merging current `origin/dev`, and the
Spring Boot fixture root `C:\craton\CratonVM-spring-boot-rerun-20260717\apps\spring-boot`:

| mode | classes | failures | aborts | skips | container failures | wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| no-JIT | 15/15 PASS | 0 | 0 | 0 | 0 | 138.160 s |
| JIT | 15/15 PASS | 0 | 0 | 0 | 0 | 109.571 s |

`ZipContentTests` completed all 29 tests in 119.1 s no-JIT and 92.7 s JIT,
both inside the required 300-second per-class timeout.  This closes the
residual 15-class loader shard; no unresolved issue document was present to
move from `docs/known-issues`.
