# ES failure family - vector updateDocument overload resolves missing

Status: OPEN

Signal:
- `java.lang.NoSuchMethodError: ... updateDocument(Lorg/apache/lucene/index/Term;Ljava/lang/Iterable;)J`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- Direct `updateDocument` NoSuchMethod FAIL rows: 1 of 1064 total FAIL rows.
- Class: `ES93HnswScalarQuantizedBFloat16VectorsFormatTests`.

Current-dev proof:
- Probe run: `es-faildocs-probe-20260709-073704`
- HotSpot `ES93HnswScalarQuantizedBFloat16VectorsFormatTests`: PASS, rc=0, 4.257s.
- CratonVM JIT `ES93HnswScalarQuantizedBFloat16VectorsFormatTests`: CRASH, rc=139, 5.623s, stderr logs the same NoSuchMethod descriptor before exit.
- CratonVM --nojit `ES93HnswScalarQuantizedBFloat16VectorsFormatTests`: FAIL, rc=1, 29.263s, 4 JUnit failures including two `updateDocument` NoSuchMethodError rows.

Additional current --nojit rows with the same pattern:
- `ES920DiskBBQBFloat16VectorsFormatTests`: FAIL, 27.471s, `NoSuchMethodError: ... ES920DiskBBQBFloat16VectorsFormatTests.updateDocument(Term, Iterable)J`.
- `ES93HnswBFloat16VectorsFormatTests`: FAIL, 47.311s, two `NoSuchMethodError: ... ES93HnswBFloat16VectorsFormatTests.updateDocument(Term, Iterable)J` rows.

Representative CratonVM warning:
- `NoSuchMethodError method="org/elasticsearch/index/codec/vectors/es93/ES93HnswScalarQuantizedBFloat16VectorsFormatTests.updateDocument(Lorg/apache/lucene/index/Term;Ljava/lang/Iterable;)J" caller="org/elasticsearch/index/codec/vectors/BaseBFloat16KnnVectorsFormatTestCase.add(...) @pc=93"`

Interpretation:
- HotSpot resolves the method path; CratonVM resolves an `updateDocument(Term, Iterable)J` descriptor on the concrete test class and reports it missing.
- This reproduces under `--nojit`, so it is likely in method lookup/linkage, interface/default/inheritance handling, or descriptor selection, not a JIT compile issue.
- The affected rows cluster in BFloat16 vector test subclasses that share base test helper logic.

Next investigation:
- Inspect the class hierarchy and bytecode around `BaseBFloat16KnnVectorsFormatTestCase.add` and the concrete `updateDocument` helpers.
- Build a narrow Java probe with the same inheritance/overload/return-descriptor shape and verify CratonVM invokes the same target as HotSpot.
