# ES FAIL family summary - non-passed rerun residuals

Status: OPEN

Purpose:
- This is the FAIL-side companion to the crash/hang docs from `es-nonpassed-rerun-20260708-191002`.
- It records the exact old FAIL-row distribution and the current-dev representative probe results used to decide which FAIL rows are CratonVM bugs.

Source run:
- Run: `es-nonpassed-rerun-20260708-191002`
- Fixture root: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch`
- Result root: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun/apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002/results/es-nonpassed-rerun-20260708-191002`
- Total classes: 2648
- PASS: 1488
- FAIL: 1064
- CRASH: 86
- HANG: 10

FAIL-row distribution:
- 1022: `System$1.findNative(ClassLoader,String)J` missing. Already represented as fixed in `docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-system1-findnative-FIXED.md`.
- 17: vector assertion failures, usually non-zero expected scores/values returned as `0.0`.
- 11: `SymbolLookup.find(String)` AbstractMethodError. Already represented as fixed in `docs/internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-symbollookup-find-abstractmethod-FIXED.md`.
- 3: Lucene vector checksum/footer corruption.
- 3: vectorization-provider warning rows that continue into vector null/linkage failures under current probes.
- 2: REST client connection/timeout rows.
- 2: BFloat16 vector value null rows.
- 2: Zstd/native-access rows. Current HotSpot also fails the representative Zstd row because the fixture lacks `libvec.so`; do not count that fixture-only signal as a CratonVM bug.
- 1: vector `updateDocument(Term, Iterable)J` NoSuchMethodError.
- 1: `FloatRandomBinaryDocValuesRangeQueryTests` read-lock unlock failure.

Current-dev representative probe:
- Probe run: `es-faildocs-probe-20260709-073704`
- Worktree: `/data/data/cratonvm-worktrees/20260709-073704-es-fail-docs`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- Elasticsearch root reused from the full run fixture.
- JDK: `/data/data/jdk25-real`.
- Selected representatives: 14 classes.
- Hang timeout: 600 seconds.

Representative matrix:
- HotSpot: 13 PASS, 1 FAIL. The single HotSpot FAIL is `Zstd814BestCompressionStoredFieldsFormatTests`, caused by the fixture missing `libvec.so`, so it is not a clean CratonVM-only signal.
- CratonVM JIT: 1 PASS, 2 FAIL, 11 CRASH. The pass is `RestClientGzipCompressionTests`; the FAIL rows are `ES93FlatVectorFormatTests` and `DiversifyingChildrenIVFKnnFloatVectorQueryTests`; the rest exit rc=139.
- CratonVM --nojit: 1 PASS, 10 FAIL, 3 HANG. The pass is `RestClientGzipCompressionTests`; the HANG rows are `ES812PostingsFormatTests`, `FloatRandomBinaryDocValuesRangeQueryTests`, and `IVFKnnFloatSlicedVectorQueryTests` at the 600s timeout.

Known-issue docs added or updated from this investigation:
- `../internal/elasticsearch-suite/ES-FAIL-FAMILY-20260709-foreign-memorylayout-varhandle-FIXED.md`
- `ES-FAIL-FAMILY-20260709-vector-score-zero.md`
- `ES-FAIL-FAMILY-20260709-vector-codec-footer-mismatch.md`
- `ES-FAIL-FAMILY-20260709-vector-value-null.md`
- `ES-FAIL-FAMILY-20260709-vector-update-document-nosuchmethod.md`
- `ES-FAIL-FAMILY-20260709-restclient-singlehost-timeout.md`
- `ES-CRASH-FAMILY-20260709-currentdev-fail-probe-rc139.md`
- `ES-HANG-20260709-currentdev-nojit-es812-postings.md`
- `../internal/ES-HANG-20260709-currentdev-nojit-float-random-binary-doc-values-range-query-FIXED.md`
- `ES-HANG-20260709-currentdev-nojit-ivfknn-float-sliced-vector-query.md`

Interpretation:
- The two largest old FAIL families were already fixed on `dev`, but current-dev probes reveal deeper residual bugs behind them.
- The remaining old FAIL rows are dominated by vector codec/runtime behavior, not randomizedtesting or LuceneTestCase harness failures.
- Several current JIT runs turn the same classes into rc=139 crashes before Java can report the underlying assertion. Use the `--nojit` rows for the cleanest Java-level bug signatures.

## Current-dev 120s full non-passed rerun

- Run: `es-nonpassed-currentdev-20260709-082115`
- Selected rows: 2649
- PASS: 7
- FAIL: 2
- CRASH: 2640
- HANG: 0
- The source non-passed list had one class that was not recorded in the old completed result TSVs: `server org.elasticsearch.index.fieldstats.FieldStatsProviderRefreshTests`.

The two FAIL rows were:
- `server org.elasticsearch.index.codec.vectors.es93.ES93FlatVectorFormatTests` -> `CorruptIndexException: codec footer mismatch`.
- `server org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatVectorQueryTests` -> `java.lang.AssertionError`.

The old 10 HANG classes were rerun separately with a 1500s timeout in `es-hung10-currentdev-20260709-082115`; all 10 became rc=139 CRASH rows and none hung.
