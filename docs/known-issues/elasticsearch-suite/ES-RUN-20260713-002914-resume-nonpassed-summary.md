# Elasticsearch non-passed resume run 20260713-002914

Status: COMPLETE, historical evidence only

## Scope

This four-shard Azure run resumed every class not recorded as PASS in the
reference TSV, including previously failed, hung, and untested classes.

- CratonVM source: `83ce0f19d2a8a14529c0a7c7388ab09ccc018299`
- Binary: `cratonvm-es-nonpassed-resume-currentdev-20260713-002914`
- JIT: on
- Per-class hang timeout: 120 seconds
- Fixture: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Reference: `cratonvm-suite/results.jit.all.tsv`

## Exact result

| Discovered | Prior PASS | Selected | PASS | FAIL | HANG | CRASH |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 2701 | 52 | 2649 | 1254 | 1259 | 131 | 5 |

All 2,649 selected rows were recorded. The detached remote finalizer wrote
the aggregate at `2026-07-13T05:02:32Z`.

## Attribution

This run does not establish new CratonVM issue families.

- 1,207 FAIL, 131 HANG, and all 5 CRASH rows carry the same
  `UnsatisfiedLinkError` for the missing fixture file
  `apps/elasticsearch/lib/platform/linux-x64/libvec.so`. This is an Azure
  fixture blocker, not a CratonVM failure. No matching library existed
  elsewhere under `/data/data` to restore safely.
- The five `CorruptIndexException: codec footer mismatch` rows belong to the
  previously tracked vector footer family.
- Forty rows are Lucene randomized-testing `Test abandoned because suite
  timeout was reached` results, already tracked as a vector-performance
  timeout family.
- The single `RestClientMultipleHostsIntegTests::testNodeSelector`
  `ConnectionClosedException` row belongs to the previously tracked REST
  connection-closed family.

The run is historical rather than a current-dev verdict: by the time this
summary was written, later `dev` commits had moved the REST and vector-footer
notes under `docs/internal` as fixed. Revalidate representative classes with
a binary built from the current tip before reopening either issue.

## Evidence

Remote result directory:

```text
/data/data/cratonvm-suite-runs/es-nonpassed-resume-currentdev-20260713-002914
```
