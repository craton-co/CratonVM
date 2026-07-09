# Elasticsearch non-passed rerun final summary - 2026-07-09

Status: COMPLETE

This is the final collection summary for the non-passed Elasticsearch rerun requested from `dev` on the Azure host. The run completed all selected rows; docs below preserve crash/hang evidence from the collection binary.

Environment:
- Host: `victor@20.83.144.174`
- Worktree: `/data/data/cratonvm-worktrees/20260708-191002-es-nonpassed-rerun`
- Branch: `codex/es-nonpassed-rerun-20260708-191002`
- Collection binary base dev SHA: `3d61003bbfdf9c6b045d29afefd45519dc558881`
- Docs generated after worktree fast-forward to dev SHA: `8736a20b6e269bae3ec89d44e22117e2d4eba9a0`
- CratonVM binary: `/data/data/cratonvm-targets/es-nonpassed-20260708-191002/release/cratonvm-es-nonpassed-20260708-191002`
- Elasticsearch fixture: `apps/elasticsearch` in the isolated worktree
- Runner work dir: `apps/elasticsearch-suite-runner/.suite-es-nonpassed-20260708-191002`
- Run name: `es-nonpassed-rerun-20260708-191002`
- Selection: `-Category others -Jit on -Vm craton`, four shards, `-TimeoutSec 600`

Final completed-row counts:
- PASS=1488
- FAIL=1064
- CRASH=86
- HANG=10
- total=2648

Shard counts:
- `jit-shard1`: total=663, PASS=377, FAIL=264, CRASH=21, HANG=1
- `jit-shard2`: total=663, PASS=433, FAIL=208, CRASH=20, HANG=2
- `jit-shard3`: total=662, PASS=258, FAIL=379, CRASH=20, HANG=5
- `jit-shard4`: total=660, PASS=420, FAIL=213, CRASH=25, HANG=2

Crash/hang doc generation:
- Newly added known-issues docs: 67 (59 crash, 8 hang)
- Existing Elasticsearch-suite docs reused/skipped: 29 (27 crash, 2 hang)
- Existing fixed docs under `docs/internal/elasticsearch-suite` were not re-opened by this final collection pass.

Top non-pass notes:
- 1112: `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
- 11: `java.lang.AbstractMethodError: method java/lang/foreign/SymbolLookup.find(Ljava/lang/String;)Ljava/util/Optional; has no Code attribute`
- 6: `<empty>`
- 4: `java.lang.AssertionError: expected:<1.0> but was:<0.0>`
- 4: `Caused by: java.lang.AssertionError: expected:<0.7245078> but was:<0.0>`
- 3: `org.apache.lucene.index.CorruptIndexException: checksum status indeterminate: remaining=0; please run checkindex for more details (resource=BufferedChecksumIndexInput(MockIndexInpu`
- 2: `java.lang.IllegalArgumentException: vector value must not be null`
- 1: `java.util.concurrent.ExecutionException: org.apache.http.ConnectionClosedException: Connection closed unexpectedly`
- 1: `java.lang.AssertionError: timeout waiting for requests to be sent`
- 1: `.[2026-07-09T06:43:53,124][WARN ][o.e.s.ESVectorizationProvider][testFieldConstructorExceptions] Java runtime is not using Hotspot VM; Java vector incubator API can't be enabled.`
- 1: `Caused by: java.lang.AssertionError: expected:<0.38698804> but was:<0.0>`
- 1: `.[2026-07-09T06:47:18,838][WARN ][o.e.s.ESVectorizationProvider][testFieldConstructorExceptions] Java runtime is not using Hotspot VM; Java vector incubator API can't be enabled.`
- 1: `java.lang.AssertionError: expected:<-1.0> but was:<0.0>`
- 1: `.[2026-07-08T22:41:24,380][WARN ][o.e.s.ESVectorizationProvider][testFieldConstructorExceptions] Java runtime is not using Hotspot VM; Java vector incubator API can't be enabled.`
- 1: `java.lang.AssertionError: expected:<3.2076945062726736> but was:<0.0>`
- 1: `Caused by: java.lang.AssertionError: expected:<0.44256574> but was:<0.0>`
- 1: `java.lang.AssertionError: expected:<0.027795367> but was:<0.012313265>`
- 1: `java.lang.NoSuchMethodError: org/elasticsearch/index/codec/vectors/es93/ES93HnswScalarQuantizedBFloat16VectorsFormatTests.updateDocument(Lorg/apache/lucene/index/Term;Ljava/lang/It`
- 1: `Caused by: java.lang.AssertionError: expected:<0.3569802> but was:<0.0>`
- 1: `Caused by: java.lang.AssertionError: expected:<0.068339564> but was:<0.0>`

New crash/hang docs added by this final pass:
- `CRASH` `libs/tdigest` `org.elasticsearch.tdigest.SortingDigestTests` -> [ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511.md](ES-CRASH-20260709-libs-tdigest-org-elasticsearch-tdigest-sortingdigesttests-0249530511.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.node.shutdown.NodesRemovalPrevalidationSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-node-shutdown-nodesremovalprevalidationserializationtests-6e79e62350.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-node-shutdown-nodesremovalprevalidationserializationtests-6e79e62350.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.node.shutdown.PrevalidateNodeRemovalResponseSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-node-shutdown-prevalidatenoderemovalresponseserializationtests-a9179653f0.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-node-shutdown-prevalidatenoderemovalresponseserializationtests-a9179653f0.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.settings.ClusterUpdateSettingsResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-settings-clusterupdatesettingsresponsetests-0c6f8e07b2.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-settings-clusterupdatesettingsresponsetests-0c6f8e07b2.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.snapshots.status.SnapshotStatusTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-snapshots-status-snapshotstatustests-f6a852c1de.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-snapshots-status-snapshotstatustests-f6a852c1de.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.storedscripts.GetScriptContextResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getscriptcontextresponsetests-0b8ba47253.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getscriptcontextresponsetests-0b8ba47253.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.storedscripts.GetStoredScriptResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getstoredscriptresponsetests-1a2b5efa27.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-getstoredscriptresponsetests-1a2b5efa27.md)
- `CRASH` `server` `org.elasticsearch.search.aggregations.metrics.InternalTopHitsTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-metrics-internaltophitstests-702f08c67d.md](ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-metrics-internaltophitstests-702f08c67d.md)
- `CRASH` `server` `org.elasticsearch.action.admin.cluster.storedscripts.ScriptContextInfoSerializingTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-scriptcontextinfoserializingtests-e2cdec4d58.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-cluster-storedscripts-scriptcontextinfoserializingtests-e2cdec4d58.md)
- `CRASH` `server` `org.elasticsearch.action.admin.indices.create.CreateIndexResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-create-createindexresponsetests-a785dc185d.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-create-createindexresponsetests-a785dc185d.md)
- `CRASH` `server` `org.elasticsearch.search.aggregations.metrics.weighted_avg.WeightedAvgAggregationBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-metrics-weighted-avg-weightedavgaggregationbuildertests-c108472773.md](ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-metrics-weighted-avg-weightedavgaggregationbuildertests-c108472773.md)
- `CRASH` `server` `org.elasticsearch.action.admin.indices.open.OpenIndexResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-open-openindexresponsetests-ebc7a582b9.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-open-openindexresponsetests-ebc7a582b9.md)
- `CRASH` `server` `org.elasticsearch.action.admin.indices.resolve.ResolveIndexResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-resolve-resolveindexresponsetests-2a4aecbadf.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-resolve-resolveindexresponsetests-2a4aecbadf.md)
- `CRASH` `server` `org.elasticsearch.index.query.CombineIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-combineintervalssourceprovidertests-f453671d1c.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-combineintervalssourceprovidertests-f453671d1c.md)
- `CRASH` `server` `org.elasticsearch.index.query.DisjunctionIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-disjunctionintervalssourceprovidertests-8c8a0f84ca.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-disjunctionintervalssourceprovidertests-8c8a0f84ca.md)
- `CRASH` `server` `org.elasticsearch.action.admin.indices.rollover.RolloverConditionsTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-rollover-rolloverconditionstests-2cb86a9cc7.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-rollover-rolloverconditionstests-2cb86a9cc7.md)
- `CRASH` `server` `org.elasticsearch.index.query.FilterIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-filterintervalssourceprovidertests-4c021a9d73.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-filterintervalssourceprovidertests-4c021a9d73.md)
- `CRASH` `server` `org.elasticsearch.search.aggregations.support.MultiValuesSourceFieldConfigTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-support-multivaluessourcefieldconfigtests-23db43e25f.md](ES-CRASH-20260709-server-org-elasticsearch-search-aggregations-support-multivaluessourcefieldconfigtests-23db43e25f.md)
- `CRASH` `server` `org.elasticsearch.index.query.FuzzyIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-fuzzyintervalssourceprovidertests-e01bb8e9b3.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-fuzzyintervalssourceprovidertests-e01bb8e9b3.md)
- `CRASH` `server` `org.elasticsearch.search.builder.PointInTimeBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-builder-pointintimebuildertests-0b55d5c654.md](ES-CRASH-20260709-server-org-elasticsearch-search-builder-pointintimebuildertests-0b55d5c654.md)
- `CRASH` `server` `org.elasticsearch.search.builder.SubSearchSourceBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-builder-subsearchsourcebuildertests-737fdab8b9.md](ES-CRASH-20260709-server-org-elasticsearch-search-builder-subsearchsourcebuildertests-737fdab8b9.md)
- `CRASH` `server` `org.elasticsearch.search.collapse.CollapseBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-collapse-collapsebuildertests-741202e74a.md](ES-CRASH-20260709-server-org-elasticsearch-search-collapse-collapsebuildertests-741202e74a.md)
- `CRASH` `server` `org.elasticsearch.index.query.MatchIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-matchintervalssourceprovidertests-7bf6763e07.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-matchintervalssourceprovidertests-7bf6763e07.md)
- `CRASH` `server` `org.elasticsearch.action.admin.indices.validate.query.QueryExplanationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-validate-query-queryexplanationtests-6acab6d561.md](ES-CRASH-20260709-server-org-elasticsearch-action-admin-indices-validate-query-queryexplanationtests-6acab6d561.md)
- `CRASH` `server` `org.elasticsearch.index.query.PrefixIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-prefixintervalssourceprovidertests-bd3281d939.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-prefixintervalssourceprovidertests-bd3281d939.md)
- `CRASH` `server` `org.elasticsearch.index.query.RangeIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-rangeintervalssourceprovidertests-dfd7cb3937.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-rangeintervalssourceprovidertests-dfd7cb3937.md)
- `CRASH` `server` `org.elasticsearch.search.fetch.subphase.FetchSourceContextTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-fetch-subphase-fetchsourcecontexttests-67962806dc.md](ES-CRASH-20260709-server-org-elasticsearch-search-fetch-subphase-fetchsourcecontexttests-67962806dc.md)
- `CRASH` `server` `org.elasticsearch.index.query.RegexpIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-regexpintervalssourceprovidertests-419f11df74.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-regexpintervalssourceprovidertests-419f11df74.md)
- `CRASH` `server` `org.elasticsearch.index.query.WildcardIntervalsSourceProviderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-query-wildcardintervalssourceprovidertests-5ad278d0a7.md](ES-CRASH-20260709-server-org-elasticsearch-index-query-wildcardintervalssourceprovidertests-5ad278d0a7.md)
- `CRASH` `server` `org.elasticsearch.search.profile.ProfileResultTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-profile-profileresulttests-0313ce7320.md](ES-CRASH-20260709-server-org-elasticsearch-search-profile-profileresulttests-0313ce7320.md)
- `CRASH` `server` `org.elasticsearch.search.profile.query.CollectorResultTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-profile-query-collectorresulttests-740c141d75.md](ES-CRASH-20260709-server-org-elasticsearch-search-profile-query-collectorresulttests-740c141d75.md)
- `CRASH` `server` `org.elasticsearch.search.profile.query.QueryProfileShardResultTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-profile-query-queryprofileshardresulttests-b0447c57f3.md](ES-CRASH-20260709-server-org-elasticsearch-search-profile-query-queryprofileshardresulttests-b0447c57f3.md)
- `CRASH` `server` `org.elasticsearch.search.profile.SearchProfileDfsPhaseResultTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-profile-searchprofiledfsphaseresulttests-5f6511fbd2.md](ES-CRASH-20260709-server-org-elasticsearch-search-profile-searchprofiledfsphaseresulttests-5f6511fbd2.md)
- `CRASH` `server` `org.elasticsearch.search.profile.SearchProfileResultsTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-profile-searchprofileresultstests-1cf6a114fb.md](ES-CRASH-20260709-server-org-elasticsearch-search-profile-searchprofileresultstests-1cf6a114fb.md)
- `CRASH` `server` `org.elasticsearch.action.explain.ExplainResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-explain-explainresponsetests-9e50335f2b.md](ES-CRASH-20260709-server-org-elasticsearch-action-explain-explainresponsetests-9e50335f2b.md)
- `CRASH` `server` `org.elasticsearch.action.fieldcaps.FieldCapabilitiesTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-fieldcaps-fieldcapabilitiestests-cf3466a32e.md](ES-CRASH-20260709-server-org-elasticsearch-action-fieldcaps-fieldcapabilitiestests-cf3466a32e.md)
- `CRASH` `server` `org.elasticsearch.action.fieldcaps.MergedFieldCapabilitiesResponseTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-fieldcaps-mergedfieldcapabilitiesresponsetests-23463acbb7.md](ES-CRASH-20260709-server-org-elasticsearch-action-fieldcaps-mergedfieldcapabilitiesresponsetests-23463acbb7.md)
- `CRASH` `server` `org.elasticsearch.index.reindex.resumeinfo.ResumeInfoWireSerializingTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-reindex-resumeinfo-resumeinfowireserializingtests-1ca23425e2.md](ES-CRASH-20260709-server-org-elasticsearch-index-reindex-resumeinfo-resumeinfowireserializingtests-1ca23425e2.md)
- `CRASH` `server` `org.elasticsearch.action.get.ShardMultiGetFromTranslogResponseSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-action-get-shardmultigetfromtranslogresponseserializationtests-b2c6a95918.md](ES-CRASH-20260709-server-org-elasticsearch-action-get-shardmultigetfromtranslogresponseserializationtests-b2c6a95918.md)
- `CRASH` `server` `org.elasticsearch.index.shard.DocsStatsSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-index-shard-docsstatsserializationtests-055e8f25e7.md](ES-CRASH-20260709-server-org-elasticsearch-index-shard-docsstatsserializationtests-055e8f25e7.md)
- `CRASH` `server` `org.elasticsearch.search.SearchSortValuesTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-searchsortvaluestests-921c1cefe3.md](ES-CRASH-20260709-server-org-elasticsearch-search-searchsortvaluestests-921c1cefe3.md)
- `CRASH` `server` `org.elasticsearch.search.vectors.KnnSearchBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-vectors-knnsearchbuildertests-0c61403646.md](ES-CRASH-20260709-server-org-elasticsearch-search-vectors-knnsearchbuildertests-0c61403646.md)
- `CRASH` `server` `org.elasticsearch.search.vectors.LookupQueryVectorBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-vectors-lookupqueryvectorbuildertests-cc3d76e784.md](ES-CRASH-20260709-server-org-elasticsearch-search-vectors-lookupqueryvectorbuildertests-cc3d76e784.md)
- `CRASH` `server` `org.elasticsearch.search.vectors.QueryVectorBuilderTests` -> [ES-CRASH-20260709-server-org-elasticsearch-search-vectors-queryvectorbuildertests-b743b806cc.md](ES-CRASH-20260709-server-org-elasticsearch-search-vectors-queryvectorbuildertests-b743b806cc.md)
- `CRASH` `server` `org.elasticsearch.inference.completion.EncryptedReasoningDetailTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-completion-encryptedreasoningdetailtests-8f48afa59c.md](ES-CRASH-20260709-server-org-elasticsearch-inference-completion-encryptedreasoningdetailtests-8f48afa59c.md)
- `CRASH` `server` `org.elasticsearch.snapshots.RegisteredPolicySnapshotsSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-snapshots-registeredpolicysnapshotsserializationtests-957715791a.md](ES-CRASH-20260709-server-org-elasticsearch-snapshots-registeredpolicysnapshotsserializationtests-957715791a.md)
- `CRASH` `server` `org.elasticsearch.inference.completion.ReasoningTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-completion-reasoningtests-d248aee64e.md](ES-CRASH-20260709-server-org-elasticsearch-inference-completion-reasoningtests-d248aee64e.md)
- `CRASH` `server` `org.elasticsearch.snapshots.RepositoriesMetadataSerializationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-snapshots-repositoriesmetadataserializationtests-0647eeb127.md](ES-CRASH-20260709-server-org-elasticsearch-snapshots-repositoriesmetadataserializationtests-0647eeb127.md)
- `CRASH` `server` `org.elasticsearch.inference.completion.SummaryReasoningDetailTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-completion-summaryreasoningdetailtests-8f38a9b2cf.md](ES-CRASH-20260709-server-org-elasticsearch-inference-completion-summaryreasoningdetailtests-8f38a9b2cf.md)
- `CRASH` `server` `org.elasticsearch.inference.completion.TextReasoningDetailTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-completion-textreasoningdetailtests-0bc312d7ef.md](ES-CRASH-20260709-server-org-elasticsearch-inference-completion-textreasoningdetailtests-0bc312d7ef.md)
- `CRASH` `server` `org.elasticsearch.inference.EmbeddingRequestTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-embeddingrequesttests-c5a8ff70c2.md](ES-CRASH-20260709-server-org-elasticsearch-inference-embeddingrequesttests-c5a8ff70c2.md)
- `CRASH` `server` `org.elasticsearch.inference.InferenceStringGroupTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-inferencestringgrouptests-9b8f6297fa.md](ES-CRASH-20260709-server-org-elasticsearch-inference-inferencestringgrouptests-9b8f6297fa.md)
- `CRASH` `server` `org.elasticsearch.snapshots.SnapshotFeatureInfoTests` -> [ES-CRASH-20260709-server-org-elasticsearch-snapshots-snapshotfeatureinfotests-766eff97c2.md](ES-CRASH-20260709-server-org-elasticsearch-snapshots-snapshotfeatureinfotests-766eff97c2.md)
- `CRASH` `server` `org.elasticsearch.inference.InferenceStringTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-inferencestringtests-3af0f39bf0.md](ES-CRASH-20260709-server-org-elasticsearch-inference-inferencestringtests-3af0f39bf0.md)
- `CRASH` `server` `org.elasticsearch.inference.RerankRequestTests` -> [ES-CRASH-20260709-server-org-elasticsearch-inference-rerankrequesttests-04f4daca42.md](ES-CRASH-20260709-server-org-elasticsearch-inference-rerankrequesttests-04f4daca42.md)
- `CRASH` `server` `org.elasticsearch.tasks.TaskInfoTests` -> [ES-CRASH-20260709-server-org-elasticsearch-tasks-taskinfotests-ba6bf27e77.md](ES-CRASH-20260709-server-org-elasticsearch-tasks-taskinfotests-ba6bf27e77.md)
- `CRASH` `server` `org.elasticsearch.cluster.coordination.AtomicRegisterCoordinatorTests` -> [ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-atomicregistercoordinatortests-55e901381f.md](ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-atomicregistercoordinatortests-55e901381f.md)
- `CRASH` `server` `org.elasticsearch.cluster.coordination.CoordinatorTests` -> [ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-coordinatortests-6cf461ca73.md](ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-coordinatortests-6cf461ca73.md)
- `CRASH` `server` `org.elasticsearch.cluster.coordination.CoordinatorVotingConfigurationTests` -> [ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-coordinatorvotingconfigurationtests-21761b42b1.md](ES-CRASH-20260709-server-org-elasticsearch-cluster-coordination-coordinatorvotingconfigurationtests-21761b42b1.md)
- `HANG` `server` `org.elasticsearch.common.cache.CacheTests` -> [ES-HANG-20260709-server-org-elasticsearch-common-cache-cachetests-f071f45f0a.md](ES-HANG-20260709-server-org-elasticsearch-common-cache-cachetests-f071f45f0a.md)
- `HANG` `server` `org.elasticsearch.search.vectors.DiversifyingChildrenIVFKnnFloatSlicedVectorQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md](ES-HANG-20260709-server-org-elasticsearch-search-vectors-diversifyingchildrenivfknnfloatslicedvectorquerytests-3ff8aa1c4b.md)
- `HANG` `server` `org.elasticsearch.search.vectors.IVFKnnFloatVectorQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-search-vectors-ivfknnfloatvectorquerytests-565afb965e.md](ES-HANG-20260709-server-org-elasticsearch-search-vectors-ivfknnfloatvectorquerytests-565afb965e.md)
- `HANG` `server` `org.elasticsearch.index.codec.tsdb.es95.ES95TSDBDocValuesFormatTests` -> [ES-HANG-20260709-server-org-elasticsearch-index-codec-tsdb-es95-es95tsdbdocvaluesformattests-ff65dd98fd.md](ES-HANG-20260709-server-org-elasticsearch-index-codec-tsdb-es95-es95tsdbdocvaluesformattests-ff65dd98fd.md)
- `HANG` `server` `org.elasticsearch.lucene.queries.DoubleRandomBinaryDocValuesRangeQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-lucene-queries-doublerandombinarydocvaluesrangequerytests-e529bd5282.md](ES-HANG-20260709-server-org-elasticsearch-lucene-queries-doublerandombinarydocvaluesrangequerytests-e529bd5282.md)
- `HANG` `server` `org.elasticsearch.lucene.queries.InetAddressRandomBinaryDocValuesRangeQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-lucene-queries-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93.md](ES-HANG-20260709-server-org-elasticsearch-lucene-queries-inetaddressrandombinarydocvaluesrangequerytests-51a9c7ea93.md)
- `HANG` `server` `org.elasticsearch.lucene.queries.IntegerRandomBinaryDocValuesRangeQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-lucene-queries-integerrandombinarydocvaluesrangequerytests-b775287812.md](ES-HANG-20260709-server-org-elasticsearch-lucene-queries-integerrandombinarydocvaluesrangequerytests-b775287812.md)
- `HANG` `server` `org.elasticsearch.lucene.queries.LongRandomBinaryDocValuesRangeQueryTests` -> [ES-HANG-20260709-server-org-elasticsearch-lucene-queries-longrandombinarydocvaluesrangequerytests-8397dce057.md](ES-HANG-20260709-server-org-elasticsearch-lucene-queries-longrandombinarydocvaluesrangequerytests-8397dce057.md)

Caveats:
- Shards 2, 3, and 4 originally aborted on a transient disk-full write while writing per-class logs; they were resumed against the same TSVs and skipped completed rows.
- The collection binary was built before later `dev` fixes that moved several earlier ES crash docs to `docs/internal/elasticsearch-suite`; use current-dev repro before assigning a final owner for any newly added open note.
- During the run, stale `target` directories older than 12h and then 8h were removed from `/data/data` to keep the suite alive; the active ES result files were not deleted.
