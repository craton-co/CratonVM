# ES failure family - vector updateDocument private invokevirtual shadow fixed

Status: FIXED

Original signal:
- `java.lang.NoSuchMethodError: ... updateDocument(Lorg/apache/lucene/index/Term;Ljava/lang/Iterable;)J`
- Representative class: `org.elasticsearch.index.codec.vectors.es93.ES93HnswScalarQuantizedBFloat16VectorsFormatTests`.
- Original caller: `BaseBFloat16KnnVectorsFormatTestCase.add(...) @pc=93`.

Root cause:
- Lucene's superclass helper is a private instance method, and modern classfiles encode that private call as `invokevirtual`.
- The concrete Elasticsearch subclass has private static helpers with the same erased name and descriptor.
- CratonVM treated the private `invokevirtual` as ordinary receiver-class dispatch, found the subclass static helper, and copied the test instance into callee local 0.
- That static helper later executed `aload_0` as if it were an `IndexWriter`, so the following `IndexWriter.updateDocument(Term, Iterable)J` call dispatched on the ES test object and surfaced as `NoSuchMethodError`.

Fix:
- `execute_invoke_kind` now detects resolved private `invokevirtual` targets and pins dispatch to the constant-pool declaring class instead of the receiver class.
- The vtable fast path yields to the slow path for these private targets, and successful private-virtual calls are not populated into the virtual receiver cache.
- Added regression test `vm/tests/private_invokevirtual_shadow.rs` for a superclass private instance helper shadowed by a subclass private static helper with the same descriptor.

Validation:
- Built unique binary: `/data/data/bin/cratonvm-es-suite-update-document-linkage-20260710-044200-r1`.
- `cargo check -p cratonvm-vm`: PASS.
- `CRATONVM_BIN=/data/data/bin/cratonvm-es-suite-update-document-linkage-20260710-044200-r1 CRATONVM_TEST_JAVA_HOME=/usr/lib/jvm/java-21-openjdk-amd64 cargo test -p cratonvm-vm --test private_invokevirtual_shadow -- --nocapture`: PASS.
- Manual one-method ES probe for `testFloatVectorScorerIteration`: no `updateDocument` NoSuchMethodError remains; it now reaches the separate `this.locals` array-length family.
- Suite wrapper row probe `probe-es-suite-update-document-linkage-r1-nojit-1358`: no `updateDocument` or `NoSuchMethodError` in logs; remaining note is `java.lang.NullPointerException: Cannot read the array length because "this.locals" is null`.

Residual:
- The same class still fails due to the separate `this.locals` / `GC-ARRAY-GUARD array_length(non-array)` issue, now tracked as `docs/known-issues/elasticsearch-suite/ES-FAIL-FAMILY-20260710-vector-locals-null-arraylength.md`.
