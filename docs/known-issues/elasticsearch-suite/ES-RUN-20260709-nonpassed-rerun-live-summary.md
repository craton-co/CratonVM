# Elasticsearch non-passed rerun live summary - 2026-07-09

Status: OPEN

This is a live investigation summary for the rerun requested from dev on the Azure host.

Environment:
- Host: victor@20.83.144.174
- Worktree: /data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun
- Branch: codex/es-nonpassed-rerun-20260708-191002
- Base branch: dev at 3d61003bbfdf9c6b045d29afefd45519dc558881
- CratonVM binary: /data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002
- Elasticsearch fixture: apps/elasticsearch in the isolated worktree, transplanted from the local compiled fixture and completed with host linux-x64 native resources
- Runner work dir: apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002
- Run name: es-nonpassed-rerun-20260708-191002
- Selection: -Category others -Jit on -Vm craton, four shards, -TimeoutSec 600, 2,649 selected classes

Current completed-row counts at doc generation time:
- CRASH=27, FAIL=298, HANG=2, PASS=424
- total=751

Top non-pass notes:
- 295: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
- 7: `java.lang.AbstractMethodError: method java/lang/foreign/SymbolLookup.find(Ljava/lang/String;)Ljava/util/Optional; has no Code attribute`
- 4: `Caused by: java.lang.AssertionError: expected:<0.7245078> but was:<0.0>`
- 4: `java.lang.AssertionError: expected:<1.0> but was:<0.0>`
- 3: `org.apache.lucene.index.CorruptIndexException: checksum status indeterminate: remaining=0; please run checkindex for more details (resource=BufferedChecksumIndexInput(MockIndexInpu`
- 2: `java.lang.IllegalArgumentException: vector value must not be null`
- 1: `.[2026-07-08T22:41:24,380][WARN ][o.e.s.ESVectorizationProvider][testFieldConstructorExceptions] Java runtime is not using Hotspot VM; Java vector incubator API can't be enabled.`
- 1: `.[2026-07-08T23:08:59,758][WARN ][o.e.n.NativeAccess       ][testRandomExceptions] Unable to load native provider. Native methods will be disabled.`
- 1: `.[2026-07-08T23:09:09,162][WARN ][o.e.n.NativeAccess       ][testRandomExceptions] Unable to load native provider. Native methods will be disabled.`
- 1: `Caused by: java.lang.AssertionError: expected:<0.068339564> but was:<0.0>`
- 1: `Caused by: java.lang.AssertionError: expected:<0.3569802> but was:<0.0>`
- 1: `Caused by: java.lang.AssertionError: expected:<0.44256574> but was:<0.0>`
- 1: `java.lang.AssertionError: expected:<-1.0> but was:<0.0>`
- 1: `java.lang.AssertionError: expected:<0.027795367> but was:<0.012313265>`
- 1: `java.lang.AssertionError: expected:<3.2076945062726736> but was:<0.0>`
- 1: `java.lang.AssertionError: timeout waiting for requests to be sent`
- 1: `java.lang.NoSuchMethodError: org/elasticsearch/index/codec/vectors/es93/ES93HnswScalarQuantizedBFloat16VectorsFormatTests.updateDocument(Lorg/apache/lucene/index/Term;Ljava/lang/It`
- 1: `java.util.concurrent.ExecutionException: org.apache.http.ConnectionClosedException: Connection closed unexpectedly`

Important caveats:
- Shards 2, 3, and 4 originally aborted on a transient disk-full write while writing per-class logs; they were resumed against the same TSVs and skip completed rows.
- The suite is still running. Regenerate this summary before final commit to capture final counts and any later crash/hang rows.
