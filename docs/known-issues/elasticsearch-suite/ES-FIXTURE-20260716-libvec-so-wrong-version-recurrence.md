# ES fixture recurrence — `libvec.so` missing again, then wrong version, in the shared 20260708 ESROOT checkout

Status: RESOLVED (fixture-only; not a CratonVM bug)

## Context

While executing the tmp-exhaustion rerun documented in
[ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md](ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md),
the shared Elasticsearch checkout used as `-ElasticsearchRoot` for that rerun —
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
— turned out to still be missing
`lib/platform/linux-x64/libvec.so`, the exact artifact that
[ES-FIXTURE-20260713-missing-libvec-so-FIXED.md](../../internal/elasticsearch-suite/ES-FIXTURE-20260713-missing-libvec-so-FIXED.md)
(2026-07-13) documented as resolved. The 07-13 fix landed in a *different*
isolated worktree/binary (`cratonvm-esfixture-libvec-land-20260715-r98`); it
never propagated the compiled `libvec.so` back into this specific,
long-lived shared checkout, which multiple ES suite runs (including the
07-15 tmp-exhaustion run and this 07-16 rerun) reuse via
`-ElasticsearchRoot`.

## Two-stage problem

**Stage 1 — file absent.** `nm`/`ls` confirmed only `libzstd.so` present in
`lib/platform/linux-x64/`. Every ES test class failed identically during
`BootstrapForTesting.<clinit>` with the same `UnsatisfiedLinkError` this
project has already characterized as fixture-only (non-CratonVM-specific,
also reachable under HotSpot).

**Stage 2 — wrong artifact copied in.** The host has *multiple*
`libvec.so` builds sitting around from the 07-13/07-14 fixture work, not all
equivalent:

| Path | Symbols | Has `*_bulk8` variants? |
| --- | ---: | :---: |
| `/data/cratonvm-esfixture-libvec-20260714/fixture-artifacts/vec-1.0.134/linux-x64/libvec.so` | 145 | No |
| `/data/cratonvm-esfixture-libvec-20260714/apps/elasticsearch/lib/platform/linux-x64/libvec.so` | 155 | Yes |
| `/data/elasticsearch-libvec-src-20260714/libs/simdvec/native/build/libs/vec/shared/amd64/libvec.so` | 155 | Yes |

The first copy attempted used the `vec-1.0.134/linux-x64` artifact (145
symbols). That resolved the "does not exist" `UnsatisfiedLinkError` but
immediately produced a *new* failure signature, reproducible on both
CratonVM and real HotSpot with the identical classpath:

```
java.lang.LinkageError: Native function [vec_cosi8_bulk8] could not be found
```

confirmed via `nm -D` that this specific `.so` build simply does not export
`vec_cosi8_bulk8` (or the other `*_bulk8` siblings) — a genuine
version/build mismatch between this compiled `libvec.so` and what the
checked-out Elasticsearch test sources (from 2026-07-08) expect, not a
CratonVM native-dispatch defect. Because `JdkVectorLibrary.<clinit>` eagerly
binds the *entire* declared native-function table at class-init time (not
lazily per call site), a single missing symbol fails class initialization
for every ES test class that reaches `NativeAccess.instance()` —
functionally universal blast radius, same shape as the stage-1 problem.

The 155-symbol build (identical content at two locations) does export all
`*_bulk8` symbols and was copied in as the final fix.

## Resolution

```bash
cp /data/cratonvm-esfixture-libvec-20260714/apps/elasticsearch/lib/platform/linux-x64/libvec.so \
   /data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch/lib/platform/linux-x64/libvec.so
```

Verified with a 20-class smoke run
(`org.elasticsearch.client.RestClientGzipCompressionTests` through
`org.elasticsearch.core.ReleasablesTests`, seed `B17AC9D3E1F2A0C4`): no
`UnsatisfiedLinkError` or `vec_cosi8_bulk8` `LinkageError` in any of the 20
classes afterward; `[main] vec_caps=3` / `Using native vector library`
logged cleanly. Remaining failures in that same smoke run are unrelated
(see [ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md](ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md)).

## Takeaway for future runs against this checkout

This shared ESROOT does not durably retain fixture fixes applied to other
worktrees/binaries — check `lib/platform/linux-x64/libvec.so` symbol count
(`nm -D ... | grep -c 'T vec_'`; should be 155, not 145 or absent) before
trusting any suite run against it. Do not assume a `Status: FIXED` doc
about this fixture applies to every checkout that shares its name pattern —
verify the specific `-ElasticsearchRoot` in use.
