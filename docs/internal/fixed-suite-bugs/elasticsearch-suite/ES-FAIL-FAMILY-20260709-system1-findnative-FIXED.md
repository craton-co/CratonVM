# ES failure family - JavaLangAccess System$1.findNative missing / JIT crash tail

Status: FIXED

Signal:
- `java.lang.NoSuchMethodError: java/lang/System$1.findNative(Ljava/lang/ClassLoader;Ljava/lang/String;)J`
- Seen in ordinary failures, all currently observed crash rows, and both current hang rows.

Current counts at doc generation time:
- FAIL: 266
- CRASH: 27
- HANG: 2

Why this is a CratonVM bug:
- HotSpot passes the representative `ClusterShardHealthTests` crash class.
- CratonVM `--nojit` also passes that representative crash class, so at least some rc=139 rows are JIT-sensitive follow-on failures rather than a pure interpreter gap.
- The JDK path in the log is `java.lang.foreign.SymbolLookup.lambda$loaderLookup$2 -> System$1.findNative(ClassLoader,String)long`.
- Remote source inspection shows CratonVM registers many `java/lang/System$1` JavaLangAccess methods in `native-builtins/src/shared_secrets_bridge.rs`, but not `findNative(ClassLoader,String)long`.
- Remote source inspection also shows `native-builtins/src/panama.rs` has synthetic `SymbolLookup.{libraryLookup,loaderLookup,find}` bridges, but the failing Elasticsearch path uses the real JDK lambda and bypasses that synthetic bridge.

Representative probe results:
- HotSpot: status=PASS, rc=0, seconds=7.525, tests=8, mode=triage-crash-hotspot
- CratonVM --nojit: status=PASS, rc=0, seconds=40.497, tests=8, mode=triage-crash-nojit

Likely fix direction:
- Add `JavaLangAccess.findNative(ClassLoader,String)long` registrations on both `java/lang/System$1` and `jdk/internal/access/JavaLangAccess`.
- Route it to CratonVM native-library/symbol lookup, probably defaulting to the loader or process lookup when the ClassLoader cannot be modeled.
- Keep the existing synthetic `SymbolLookup.find` bridge; this issue is specifically the real-JDK `loaderLookup` lambda path.
- Investigate the JIT-only rc=139 tail separately: some current crash logs include `gen_heap::get_field` OOB warnings before exit, so the missing bridge is not the whole crash mechanism.


Fixed in codex/es-fixture-20260708-220010:
- Added JavaLangAccess.findNative(ClassLoader,String) on both java/lang/System$1 and jdk/internal/access/JavaLangAccess.
- Routed SymbolLookup/JavaLangAccess native lookup through CratonVM's native-symbol resolver instead of falling through to NoSuchMethodError.
- Verification representative: ClusterShardHealthTests, CratonVM JIT on, probe-es-fixture-20260708-220010-clustershard-r3 -> PASS, 8 tests, 0 failed.
