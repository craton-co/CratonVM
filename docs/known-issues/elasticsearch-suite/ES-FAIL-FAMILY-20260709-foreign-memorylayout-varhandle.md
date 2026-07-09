# ES failure family - MemoryLayout.varHandle has no code

Status: OPEN

Signal:
- `java.lang.AbstractMethodError: method java/lang/foreign/MemoryLayout.varHandle([Ljava/lang/foreign/MemoryLayout$PathElement;)Ljava/lang/invoke/VarHandle; has no Code attribute`

Representative class:
- `libs/cli-terminal org.elasticsearch.cli.terminal.JsonTerminalTests`

Current-dev proof:
- Probe run: `es-faildocs-probe-20260709-073704`
- Binary: `/data/data/cratonvm-targets/20260709-073704-es-fail-docs/release/cratonvm-20260709-073704-es-fail-docs`
- HotSpot: PASS, rc=0, 2.355s.
- CratonVM JIT: CRASH, rc=139, 1.824s, note contains the `MemoryLayout.varHandle` AbstractMethodError.
- CratonVM --nojit: FAIL, rc=1, 6.438s, same `MemoryLayout.varHandle` AbstractMethodError before JUnit initialization failures.

Stack anchor from CratonVM --nojit stdout:
- `org.elasticsearch.nativeaccess.jdk.JdkZstdLibrary.<clinit>(JdkZstdLibrary.java:91)`
- `org.elasticsearch.nativeaccess.lib.NativeLibraryProvider.getLibrary(NativeLibraryProvider.java:56)`
- `org.elasticsearch.nativeaccess.NativeAccessHolder.<clinit>(NativeAccessHolder.java:29)`
- `org.elasticsearch.bootstrap.Elasticsearch.initializeNatives(Elasticsearch.java:479)`
- `org.elasticsearch.test.ESTestCase.<clinit>(ESTestCase.java:365)`

Counts:
- Old full rerun direct FAIL rows: 0 explicit `MemoryLayout.varHandle` rows. The old rerun was mostly blocked earlier by `System$1.findNative` and `SymbolLookup.find` failures.
- Current representative probe: 1 HotSpot-passing class with the clean signal (`JsonTerminalTests`).
- Supporting current probe: `Zstd814BestCompressionStoredFieldsFormatTests` also hits the same CratonVM AbstractMethodError, but the HotSpot representative fails too because the fixture lacks `libvec.so`, so it is supporting evidence only.

Interpretation:
- This is a CratonVM foreign-memory API coverage gap: `MemoryLayout.varHandle(PathElement...)` is visible but has no executable implementation.
- The failure happens during Elasticsearch native-access bootstrap, before the selected test class can run.
- It reproduces without JIT, so the root is not a JIT compile-time issue.

Next investigation:
- Implement or bridge `java/lang/foreign/MemoryLayout.varHandle([MemoryLayout$PathElement])VarHandle` consistently with the real JDK 25 foreign-memory API.
- Add a narrow FFM regression before relying on the Elasticsearch suite, because many ES classes initialize `NativeAccess` and can mask the same missing method behind unrelated test names.
