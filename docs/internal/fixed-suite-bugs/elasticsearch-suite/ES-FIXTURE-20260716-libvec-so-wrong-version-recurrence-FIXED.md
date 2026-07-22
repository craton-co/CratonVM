# ES fixture recurrence — `libvec.so` missing again, then wrong version, in the shared 20260708 ESROOT checkout

Status: RESOLVED (fixture-only; not a CratonVM bug)

## Context

While executing the tmp-exhaustion rerun documented in
[ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md](../../known-issues/elasticsearch-suite/ES-RUN-20260715-root-tmp-exhaustion-invalidates-rerun.md),
the shared Elasticsearch checkout used as `-ElasticsearchRoot` for that rerun —
`/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
— turned out to still be missing
`lib/platform/linux-x64/libvec.so`, the exact artifact that
[ES-FIXTURE-20260713-missing-libvec-so-FIXED.md](-so-FIXED.md)
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
(see [ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md](../../known-issues/elasticsearch-suite/ES-BUG-20260716-embeddedimplclassloader-noclassdeffounderror.md)).

## Durable closure (2026-07-17)

The fixture is now rebuilt from the exact checked-out Elasticsearch native
sources, not copied from another worktree or artifact cache. The host rebuild
exports 155 `vec_*` symbols and includes the required `bulk8` ABI sentinels.
A HotSpot FFM probe loaded the installed library, resolved `vec_caps`,
`vec_cosi8_bulk8`, `vec_doti8_bulk8`, and `vec_sqri8_bulk8`, and successfully
invoked `vec_caps`.

`apps/elasticsearch-suite-runner/prepare-elasticsearch-libvec-fixture.ps1`
now performs the reproducible build, export check, and atomic installation.
`run-elasticsearch-suite.ps1` validates the same ABI before selecting any test
classes, so an absent or stale fixture is never recorded as a suite-wide
CratonVM failure.

## Takeaway for future runs against this checkout

Run the fixture preparation script for each exact `-ElasticsearchRoot`; do not
copy `libvec.so` between checkouts. This shared ESROOT does not durably retain fixture fixes applied to other
worktrees/binaries — check `lib/platform/linux-x64/libvec.so` symbol count
(`nm -D ... | grep -c 'T vec_'`; should be 155, not 145 or absent) before
trusting any suite run against it. Do not assume a `Status: FIXED` doc
about this fixture applies to every checkout that shares its name pattern —
verify the specific `-ElasticsearchRoot` in use.
