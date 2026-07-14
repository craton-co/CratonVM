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

Rebuild or restore the native Linux artifacts for the exact compiled
Elasticsearch fixture, including `libvec.so`, then rerun the 1,343 affected
classes. Preserve the result classification separately from real CratonVM
FAIL/HANG/CRASH families; this blocker otherwise masks nearly the entire
suite screen.

## Evidence

- Run summary:
  `ES-RUN-20260713-002914-resume-nonpassed-summary.md`
- Remote results:
  `/data/data/cratonvm-suite-runs/es-nonpassed-resume-currentdev-20260713-002914`
