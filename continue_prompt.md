# CratonVM — continue from here

CratonVM is a Rust JVM at `C:\craton\CratonVM`. Work branch is **`dev`**.
JDK used for testing: **`C:/Program Files/Java/jdk-25`** (25.0.1).
Build command: `cargo build --release -p cratonvm-cli` (≈5–10 min from cold,
≈3 min incremental). Binary: `target/release/cratonvm.exe`.

**Project constraints (hard rules — see `apps/TARGET_APPS.md` and
`memory/feedback_no_synthetic_stubs.md`):**

- **No synthetic stubs.** When a Java class is missing or a method has no
  body, do NOT short-circuit with a Rust shim that fakes the answer.
  Fix the real bytecode/native dispatch instead.
- **No allow-list entries to mask bugs.** Force-native-override
  (`vm/src/runtime/interpreter.rs:force_native_over_real_jdk_bytecode`)
  is reserved for cases where the real JDK code has a sound-but-different
  contract from our impl. Adding entries to dodge bugs is forbidden.
- **Commit to `dev`.** All work, including agent worktrees. Pull before
  starting (`git pull origin dev`), push when done.
- **Regression pool must stay green.** Run
  `bash test-infra/regression-pool/run.sh` before pushing — all 14
  probes must PASS.

---

## Open issues (priority order)

### 1. #23 — ECJ JIT header-overwrite bug

**Symptom.** `IllegalArgumentException: / by zero` at
`org/eclipse/jdt/internal/compiler/util/HashtableOfInt.put` bci=8 when
running ECJ BatchCompiler. The `irem` op is dividing by `keyTable.length`,
which is 0.

**Repro.** ecj.jar at `C:/craton/CratonVM/apps/eclipse/ecj.jar`. Probe at
`C:/Users/Victor/AppData/Local/Temp/ecjprobe/Repro.java` (any minimal
`BatchCompiler.compile(...)` call). Run:
```sh
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  -c "C:/craton/CratonVM/apps/eclipse/ecj.jar;C:/Users/Victor/AppData/Local/Temp/ecjprobe" Repro
```

**Diagnostic results from prior session:**
- `--nojit` → bug disappears (ECJ runs to `ok=false`). Confirmed JIT-only.
- `CRATONVM_JIT_BISECT_SKIP='org/eclipse/jdt/'` → still fails. So bug is
  in JIT of some non-ECJ class (JDK class).
- `CRATONVM_JIT_BISECT_ONLY='Repro'` → bug disappears. Confirms JDK-side JIT.
- `CRATON_JIT_PFO_TRACE=1` shows the corrupt put:
  `val_cid=0 val_kind=0 val_arrlen=0x01010101` — TLAB-leftover byte pattern.
- `CRATON_JIT_NEWARRAY_TRACE=1` → **zero firings**. So `jit_newarray`
  helper is NOT called for this allocation — JIT bypasses it via some
  inline alloc path I couldn't locate from grep.
- GC-ARRAY-GUARD reads the object header LATER and finds
  `class_id=64 kind_byte=0 elem_byte=0 stored_len=0` — so the bytes
  shifted between putfield and arraylength.

**My current theory.** PFO trace at putfield captures `val_cid=0`
(correct for an `int[]`). GC-ARRAY-GUARD later reads `class_id=64`
(wrong). **The object's header is being OVERWRITTEN between putfield
and arraylength.** Suggests JIT-compiled code holds a stale pointer
across a GC move, OR a write barrier is missing. Same family as
BinTrees-18 bug fixed defensively in `bcd70d0` (walker treats
`kind=Object && array_length != 0` as corruption sentinel).

**Defensive fix already landed** (`a417674`): inline TLAB `new` path
+ `jit_post_tlab_init` now explicitly zero offset 4 (kind) and offset
12 (array_length). Does NOT close the bug — the symptom persists
because the overwrite happens downstream.

**Next concrete steps.**
- Attach lldb/windbg with a watchpoint on offset 0 of the corrupted
  object (catch when `class_id` flips from 0 to 64).
- Audit IR-tier (`jit/src/ir_lower.rs`, `jit/src/escape_analysis.rs`)
  for any inline-alloc shortcut that skips header init or short-
  circuits jit_newarray. Note: `ir_lower.rs` has NO handler for
  `Op::NewArray` even though `ir.rs:221` declares it — the IR path
  for newarray-containing methods bails out via `_ => return None`
  in the IR builder at `jit/src/ir.rs:944`, falling back to bytecode-x64.
  So if jit_newarray isn't being called, look at the bytecode-x64
  newarray opcode handler `jit/src/x64.rs:16172` — it does call
  `self.helpers.newarray`, but maybe a method-level skip or cache hit
  is bypassing it.
- Audit `try_jit_upgrade_with_gate` (tier-2 promotion) — the OSR
  field_info fix at `de387c1` was for tier-1 OSR; tier-2 might have a
  similar gap. Search `vm/src/runtime/interpreter.rs` for any other
  `Vec::new()` argument to `crate::jit::x64::compile(`.

**Diagnostic env vars to combine** in the same run:
```
CRATON_JIT_PFO_TRACE=1     # putfield_object: log obj, val, val_cid, val_kind, val_arrlen
CRATON_JIT_NEWARRAY_TRACE=1 # jit_newarray: log ptr, len, header
CRATONVM_GC_ARRAY_GUARD_BT=1 # Rust backtrace at the array_length(non-array) site
DISABLE_INLINE_GETFIELD=1   # bypass inline getfield codegen (bisects whether getfield is involved)
```

---

### 2. H2 TestAll — hangs at 240-min SelfDestructor

**Status as of session end.** `2045ff5` fixed the `Properties.remove`
always-null stub, which was triggering "Unsupported connection setting
USER" on EVERY H2 JDBC connect. With that fix, H2 should boot deeper
into the test set. **A fresh CratonVM run still hung 240 minutes** —
H2's built-in SelfDestructor killed it. Log only got past the banner.

**Repro:**
```sh
cd C:/craton/CratonVM/apps/_test-suites/h2database/h2
CP='temp;ext/jts-core-1.19.0.jar;ext/jakarta.servlet-api-5.0.0.jar;ext/javax.servlet-api-4.0.1.jar;ext/asm-9.5.jar;ext/lucene-core-9.7.0.jar;ext/lucene-analysis-common-9.7.0.jar;ext/lucene-queryparser-9.7.0.jar;ext/slf4j-api-2.0.7.jar;ext/junit-jupiter-api-5.10.0.jar;ext/apiguardian-1.1.2.jar;ext/org.osgi.core-5.0.0.jar;ext/org.osgi.service.jdbc-1.1.0.jar'
C:/craton/CratonVM/target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx1g -cp "$CP" org.h2.test.TestAll
```

**Diagnostic plan.**
- First, verify minimal repro from the agent's fix:
  ```java
  // Should print: "OK: 2.4.249-SNAPSHOT" — Properties.remove fix landed in 2045ff5
  DriverManager.getConnection("jdbc:h2:mem:test;USER=sa")
  ```
- Then verify a single H2 test class runs (`TestAlter` etc.). If
  individual tests work but `TestAll` hangs, there's a per-test
  state-bleed bug (TestAll runs 300 classes in one JVM).
- If the hang is during `runAddedTests()` (the multi-threaded test
  runner — uses `ManagementFactory.getOperatingSystemMXBean().getAvailableProcessors()`),
  it could be another sync/locking issue similar to the
  `IllegalMonitorStateException` we saw in `Utils.collectGarbage`
  earlier. That exception still fires; it's currently non-fatal but
  worth investigating.

**HotSpot baseline:** 133 s wall, 4 upstream FAILs (TestFunctions,
TestPreparedStatement, TestCrashAPI, TestOutOfMemory — all H2/JDK25
incompats unrelated to CratonVM).

---

### 3. Bouncy Castle :core:test — EncodedStream field-layout mismatch

**Agent #33 diagnosed.** With System.in fixed (`7ce34a2`), Gradle's
test worker boots and starts decoding stdin via
`worker.org.gradle.internal.stream.EncodedStream$EncodedInput`. CratonVM's
`get_field` is asked for slot 1, but the class declares `num_slots=1`
(only field is `delegate`). Cratonvm's "out-of-bounds field read
dropped" guard fires thousands of times; the Gradle worker eventually
hangs and the parent disconnects after 120 s.

**Likely cause.** Stale inline cache (IC) after an upcast/downcast in
JIT-compiled getfield codegen. See `vm/src/runtime/interpreter.rs`
getfield path; cratonvm-vm's bounds-check guard is in
`set_field`/`get_field`.

**Direct-invocation fallback exists.** `apps/_test-suites/bc-java`
has a `run-bc-core-cratonvm.sh` style approach in the agent's
worktree (`.claude/worktrees/agent-a61f77a51ce4e0073/test-infra/run-bc-core-cratonvm.sh`)
— ran 18 AllTests suites in 562 s, 3/18 complete cleanly. The other
15 die early from a mix of: heap corruption (`class_id=3540375540` in
walker — same JIT family as #23), EC math errors (see #4),
120 s watchdog on `pqc.crypto.*`.

---

### 4. Bouncy Castle crypto — SM2/EC "Fp q value not prime"

**Surfaced by agent #34** after the RSA BigInteger fix (`958baae`)
unblocked RegressionTest's `<clinit>` past the RSA check. Now dies in
`SM2SignerTest.<clinit>` with `"Fp q value not prime"`. The
elliptic-curve prime-field check rejects a value that HotSpot accepts.

**Probable root cause: same family as the RSA bug.** Agent #34 noted:
> `native-builtins/src/phases_late.rs::register_p71_biginteger_extras`
> (lines ~34767–35100) registers a second wave of BigInteger natives
> (`shiftRight`, `testBit`, `modPow`, `gcd`, `and/or/xor/not`, ...)
> that all go through `p71_bi_val = bi_read().parse::<i128>().unwrap_or(0)`
> — same family of bug for values > 128 bits.

**Fix pattern is established.** See `958baae` — read directly from the
real-JDK `mag:[I` layout via the new `bi_low_bits` /
`bi_low_twos_complement` helpers in `native-builtins/src/lib.rs`
(~lines 23900-23990) instead of decimal-string-parse-with-unwrap_or(0).
Apply the same pattern to every other BigInteger op in `phases_late.rs`.

---

### 5. Bouncy Castle ASN.1 — data-correctness gaps

**Status:** 38/58 OK after agent #35's Calendar skip-list fix
(`cbda1a0`). The remaining 20 failures are NOT JIT crashes — they're
pre-existing data-correctness gaps:

- `DERT61String.getString() result incorrect` — string-decoding bug in
  CratonVM's char decoder. Look at how CratonVM decodes T.61
  (8-bit Latin) byte arrays into `String`.
- "Unknown object id - cn / CN / o / businessCategory" — BC's
  X.500/LDAP attribute-name registry isn't populating. Almost
  certainly a `<clinit>` ordering issue similar to the H2 USER bug:
  a static `HashMap` somewhere isn't getting filled in CratonVM
  during class initialization. Look for `RFC4519`, `X509Name`,
  `BCStyle`, etc. classes in BC; trace their static initializers.
- `ObjectIdentifier: Should be taken from cache` — OID caching
  divergence.
- `X509Alt: Exception: unexpected object: org.bouncycastle.asn1.DERBitString`
  — ASN.1 type-tag confusion.

**Repro:**
```sh
cd C:/craton/CratonVM/apps/_test-suites/bc-java
CP='core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test'
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx1g -cp "$CP" org.bouncycastle.asn1.test.RegressionTest
```

---

### 6. Bouncy Castle math.ec — EC math + GC corruption

**Agent #36 set up the JUnit-3 harness.** HotSpot baseline: 14 tests
OK, 49 s. CratonVM: 4 tests in 54.5 s then crashes with TWO bugs:
1. GC walker hits inline-alloc header corruption
   (`kind=Object && array_length=512`) → arena re-sync (same family as #23).
2. Speculative collection-layout probe dispatched on `java/lang/Object`
   (slot index past layout) → `NoSuchMethodError: java/lang/Object.hasNext()Z`
   → `InternalError: JIT dispatch into junit/textui/TestRunner.start failed`.

**Setup is documented** in agent #36's transcript. junit-3.8.2.jar is
at `C:/Users/Victor/AppData/Local/Temp/junit-3.8.2.jar`. Invocation
needs Windows paths (`cygpath -w`) and uses
`junit.textui.TestRunner` as main with `AllTests` as argument.

---

### 7. BigInteger parse-decimal bug family (follow-up to #4)

Agent #34's commit message explicitly flagged this:

> `native-builtins/src/phases_late.rs::register_p71_biginteger_extras`
> (lines ~34767–35100) registers a second wave of BigInteger natives
> (`shiftRight`, `testBit`, `modPow`, `gcd`, `and/or/xor/not`, ...)
> that all go through `p71_bi_val = bi_read().parse::<i128>().unwrap_or(0)`
> — same family of bug for values > 128 bits. Likely contributing to
> the SM2 / future failures. Worth a follow-up.

Worth a single sweep: grep for `bi_read()` and `parse::<i128>` in
phases_late.rs, rewrite each to use the `bi_low_bits` /
`bi_low_twos_complement` helpers added by `958baae`.

---

## Test-suite passing status

### Passes today
| Suite | CratonVM | HotSpot | Notes |
|---|---:|---:|---|
| Commons Math (3204 tests) | 113 s | 114 s | byte-identical; CratonVM-GPU 75 s |
| Regression pool (14 probes) | ~20 s | n/a | committed baselines, must stay green |
| BC ASN.1 RegressionTest | 38/58 | 58/58 | up from 20+SEGFAULT before `cbda1a0` |

### Setup needed before retrying
| Suite | What's needed |
|---|---|
| H2 TestAll | After `2045ff5` Properties.remove fix, why does TestAll still hang at 240-min watchdog? Run minimal `DriverManager.getConnection` probe first to confirm the fix; then sub-tests individually. |
| BC :core:test (555 tests) | Either fix the EncodedStream IC bug (#3) so gradle worker survives, or use the agent's direct-runner approach (run AllTests classes one-by-one) and capture pass counts per suite. |
| BC crypto RegressionTest | Fix the SM2 EC primality issue (#4) by addressing the BigInteger parse-decimal family (#7). |
| Spring Boot / Tomcat / WildFly / Keycloak | Full source trees in `apps/{spring-boot,tomcat,wildfly,keycloak}`. Build systems: Gradle (sb), Ant (tomcat), Maven (wildfly, keycloak). Tomcat needs `ant` installed (not on PATH). All ~30+ min build cycles before tests can run; none attempted yet. |

---

## Diagnostic knobs catalog

```
# JIT instrumentation (vm/src/jit/helpers.rs)
CRATON_JIT_PFO_TRACE=1        # putfield_object: log every JIT-emitted call
CRATON_JIT_PFI_TRACE=1        # putfield_int: same for int-typed fields
CRATON_JIT_NEWARRAY_TRACE=1   # jit_newarray helper: log every primitive-array alloc

# GC instrumentation (gc/src/gen_heap.rs)
CRATONVM_GC_ARRAY_GUARD_BT=1  # Rust backtrace at array_length(non-array) warning site

# JIT control
CRATONVM_DISABLE_JIT=1        # disable JIT entirely (interpreter only)
--nojit                       # CLI flag, same as above
CRATONVM_JIT_BISECT_SKIP='pattern1,pattern2'  # skip JIT compilation for class-name prefixes
CRATONVM_JIT_BISECT_ONLY='pattern1,pattern2'  # only JIT-compile matching class names

# Inline-getfield codegen (jit/src/x64.rs)
DISABLE_INLINE_GETFIELD=1     # bypass inline getfield codegen; use resolved-field-info path

# CDS (sometimes interferes — leave off by default; the launcher uses --Xshare off)

# Stack-dump watchdog (the CRATONVM-side, NOT H2's)
--stack-dump-on-timeout 0     # disable cratonvm's 120s soft-abort with stack dump
                              # ALWAYS pass this for long-running tests (BC, H2)

# Other
CRATONVM_DBG_ATHROW=1         # log every athrow
CRATONVM_DBG_CHARSET=1        # dump live Java thread stack at charset-NPE throw time
```

---

## Cookbook — common workflows

### Run a Java probe under CratonVM with the canonical flags

```sh
C:/craton/CratonVM/target/release/cratonvm.exe \
  --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 \
  -Xmx1g \
  -cp "your;cp;here" \
  YourMainClass
```

### Run a Maven test suite under CratonVM (using the Surefire shim)

```sh
JAVA_HOME="C:/Program Files/Java/jdk-25" \
  /c/tools/apache-maven-3.9.15/bin/mvn -B -ntp \
  -Djvm='C:\craton\CratonVM\test-infra\cratonvm-java-shim.bat' \
  test
```

Shims available in `test-infra/`:
- `cratonvm-java-shim.bat` — CPU mode
- `cratonvm-gpu-java-shim.bat` — `--gpu` flag added
- `tornadovm-java-shim.bat` — TornadoVM JDK 25.0.3 with Graal + PTX argfile

### Bisect a JIT bug

```sh
# Step 1: confirm it's JIT-related
... --nojit ...

# Step 2: if --nojit works, identify which classes' JIT is the culprit
CRATONVM_JIT_BISECT_ONLY='your/main/class' ...      # works → bug is in some OTHER class's JIT
CRATONVM_JIT_BISECT_SKIP='java/util/Calendar' ...    # works → that class is the culprit

# Step 3: once a method is suspected, trace its emissions
CRATON_JIT_PFO_TRACE=1 ... 2>&1 | grep PFO

# Step 4: if confirmed but you can't fix the codegen, add to the skip_list
# vm/src/jit/skip_list.rs: is_known_miscompile
```

### Build + test cycle

```sh
cd C:/craton/CratonVM
git pull origin dev
cargo build --release -p cratonvm-cli   # ~5 min incremental
bash test-infra/regression-pool/run.sh  # MUST stay 14/14 PASS
# ... your test ...
git add <only the files you changed>
git commit -m "subsystem: short description ..."
git push origin dev
```

---

## File-paths cheat sheet

```
C:\craton\CratonVM\
├── vm/src/runtime/interpreter.rs          # opcode dispatch, force-native allow-list, OSR
├── vm/src/jit/helpers.rs                  # JIT helper functions (jit_newarray, jit_put*, jit_post_tlab_init, …)
├── vm/src/jit/skip_list.rs                # is_known_miscompile — JIT skip patterns
├── jit/src/x64.rs                         # bytecode-x64 codegen (newarray at line 16172, new at 16197, inline TLAB at 7189)
├── jit/src/ir.rs                          # IR builder; rejects unsupported opcodes with `_ => return None`
├── jit/src/ir_lower.rs                    # IR-tier lowering; NO handler for Op::NewArray
├── jit/src/escape_analysis.rs             # classifies allocations; NewArray cannot be scalar-replaced
├── gc/src/vm_heap.rs                      # VmHeap dispatch (Generational vs G1)
├── gc/src/gen_heap.rs                     # GenerationalHeap; defensive walker at bcd70d0
├── native-builtins/src/lib.rs             # huge — most natives live here, including the new bi_low_bits helpers
├── native-builtins/src/jmx.rs             # JMX MXBeans (GC count fix at cc5efa4)
├── native-builtins/src/phases_late.rs     # phase-N BigInteger natives (parse-decimal bug family)
├── native-builtins/src/properties_sidetable.rs  # Properties side-table (remove fix at 2045ff5)
├── native-builtins/src/lang_class.rs      # Class reflection natives
├── test-infra/regression-pool/            # the 14-probe permanent suite; pool.tsv, run.sh, baselines/
├── test-infra/{cratonvm,cratonvm-gpu,tornadovm}-java-shim.bat   # Surefire-compatible JVM shims
├── apps/                                  # gauntlet apps (gitignored); test-suite clones; TARGET_APPS.md
├── apps/_test-suites/{bc-java,h2database,commons-math,bc-test-data}  # cloned test repos
└── apps/{eclipse,keycloak,spring-boot,tomcat,wildfly}  # full source clones for future test runs
```

---

## Recent commits (last session, dev tip = `7f92446`)

```
7f92446 test-infra: TornadoVM `java` shim for 4-way Surefire comparison
cbda1a0 jit: skip-list Calendar.isFieldSet (BC ASN.1 RegressionTest SEGFAULT)
958baae bigint: native intValue/longValue must return low 32/64 bits, not parse decimal
7ce34a2 io: wire FileDescriptor on synthetic System.in so stdin reads work
2045ff5 native-builtins: side-table-aware Properties.remove(Object)  (fixes every H2 JDBC connect)
a417674 jit: defensive header zero-init in inline-new + tlab_post_init paths
952b1c1 stream+diag: register Stream.collect 3-arg native + JIT_NEWARRAY_TRACE
bcd70d0 GC: kind/array_length coherence invariant in walker size compute
cc5efa4 jmx+gc: GarbageCollectorMXBean.getCollectionTime ticks on System.gc
94f1f53 build-fix: dedup HUMONGOUS_YOUNG_FRACTION_PERCENT
053ecac regression-pool: force LF line endings + strip CR from baseline_file
bbcb908 reflection: getDeclaredClasses0 loads inner classes on demand
```

Pick the highest-impact open issue you can close in your session, fix
the underlying bug (no synthetic stubs), commit & push to `dev`,
ensure the regression pool still passes.
