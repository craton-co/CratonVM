# Elasticsearch fixture blocker - missing libvec.so

Status: FIXED fixture blocker

Date observed: 2026-07-13

## Missing artifact

The compiled Elasticsearch fixture used by the Azure suite runner contains:

```text
apps/elasticsearch/lib/platform/linux-x64/libzstd.so
```

but lacks the required sibling:

```text
apps/elasticsearch/lib/platform/linux-x64/libvec.so
```

No `libvec.so` file existed elsewhere under `/data/data` to restore safely.
Do not substitute a library from a different Elasticsearch revision or build.

## Impact

In the complete four-shard non-passed resume run
`es-nonpassed-resume-currentdev-20260713-002914`, the missing library
produced one repeated root signature:

```text
java.lang.UnsatisfiedLinkError: Native library
[/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch/lib/platform/linux-x64/libvec.so]
does not exist
```

Affected result rows:

| FAIL | HANG | CRASH | Total |
| ---: | ---: | ---: | ---: |
| 1207 | 131 | 5 | 1343 |

The runner records five rows as `CRASH`, but every one carries this same
fixture error. They must not be attributed to a CratonVM native crash until
the fixture is repaired and the affected classes are rerun.

The same missing-library warning is reachable on HotSpot against this
fixture, so it is not a CratonVM-only signal.

## Resolution

Resolved in the Elasticsearch fixture/runtime path. The exact compiled fixture
now supplies apps/elasticsearch/lib/platform/linux-x64/libvec.so; CratonVM
loads that explicit third-party library through the JDK 25 System.load path
rather than suppressing it with the management-library compatibility shim.

The FFM bridge now represents DowncallHandle as a MethodHandle, preserves its
full descriptor/options metadata, supports heap-backed float segments, and
dispatches signature-polymorphic downcalls through libffi. Elasticsearch's
optional process-wide seccomp sandbox is treated as unavailable on CratonVM so
it does not prevent native-vector tests from starting.

The original 1,343-row result classification remains historical and must be
rerun separately; this issue no longer justifies attributing those rows to a
missing libvec.so fixture artifact.

## Evidence

- Azure isolated worktree binary:
  cratonvm-esfixture-libvec-land-20260715-r98
- JDKVectorLibraryFloat32Tests.testRandomFloats: OK (62 tests) with
  Using native vector library.
- Final r98 checks: all 62 parameters passed for testFloat32Bulk,
  testFloat32BulkWithOffsets, testFloat32BulkWithOffsetsAndPitch, and
  testFloat32BulkWithOffsetsHeapSegments.
- Exact testBulkOffsetsOutOfRange contract on r98: DIRECT_OUT_OF_RANGE_OK=62.


- Run summary:
  `ES-RUN-20260713-002914-resume-nonpassed-summary.md`
- Remote results:
  `/data/data/cratonvm-suite-runs/es-nonpassed-resume-currentdev-20260713-002914`
