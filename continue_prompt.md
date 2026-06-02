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

## Session 2026-05-28/29 — current knowledge base (READ FIRST)

dev tip = `55ca37f`. Regression pool 14/14 PASS, ~19–31 s wall (high
variance — concurrent builds skew it; anything in 18–45 s is noise).
Build the binary before any test run.

### THE SINGLE HIGHEST-LEVERAGE BUG: the dispatch-bypass family

Three separately-reported failures share **one root cause**, so one fix
closes all of them (plus ~8 DaCapo benchmarks):

- **Tomcat NIO Selector** (`docs/tomcat-selector-investigation.md`)
- **DaCapo Lucene `FSDirectory.sync` → RAF/FileDescriptor**
  (`docs/bc-ec-mod-mododdinverse-investigation.md` is the *long-bit* doc;
  the RAF angle is in commit `e2031a2`)
- **8+ DaCapo benchmarks** firing the `gen_heap::get_field` "speculative
  collection-layout probe dispatched on a non-matching receiver type"
  warning (avrora, pmd, lusearch, fop, eclipse, tomcat, batik, …).

**The mechanism.** We register natives for `Selector.open()`,
`RandomAccessFile.<init>`, etc., but during these runs **the natives
never fire** (proven with `CRATONVM_DBG_SEL=1` / `CRATONVM_DBG_RAF_GETFD=1`
— zero trace lines across a full Tomcat / luindex boot). The app's
bytecode reaches the real-JDK implementation instead:
`Selector.open()` → `SelectorProvider.provider().openSelector()` →
real-JDK `WindowsSelectorImpl`, whose platform natives (`poll0`,
`setupPipe0`) we don't implement → the selector reports closed →
continuous `ClosedSelectorException`. Same shape for RAF: a real-JDK
`RandomAccessFile` is constructed whose `fd` field we never populate,
so `getFD()` returns null and `.sync()` NPEs.

**Why it surfaces as an OOBFIELD warning.** When the wrong (real-JDK)
class is instantiated through a path we didn't intercept, a downstream
reflective / collection-probe reads a slot index past the synthetic
object's `num_slots`. The `gen_heap::get_field` guard catches it and
returns `Value::Object(None)` (benign null) — but the null then NPEs or
mis-dispatches one frame up.

**Three fix paths, in increasing order of work** (full detail in
`docs/tomcat-selector-investigation.md`):
1. Intercept `SelectorProvider.provider()` / the relevant factory so our
   synthetic implementation is returned (small, but must cover sibling
   factory methods).
2. **Debug *why* the registered `Selector.open()` static native is
   bypassed** — add a trace to the native-dispatch table lookup itself
   (not just the registered fn). Suspects: class-loader keying mismatch,
   interpreter inlining the 1-line `open()` body, or an `<clinit>`-time
   intercept that runs before our registration phase. **This is the
   recommended starting point** — it's the cheapest way to learn whether
   the bypass is general (affects every static native) or specific.
3. Implement the real-JDK `WindowsSelectorImpl` / RAF native surface
   (most correct, most work).

### bc-math-raw — the FAST reproducer for the long-bit-collision bug

`bc-math-raw` fails in **1.3 s** with JUnit
`expected:<1158644549939187780> but was:<1158644549939187780>` — the two
longs *print identically* but `equals()` says they differ. This is the
exact symptom of the `CompactValue` long↔int NaN-box collision documented
in `docs/bc-ec-mod-mododdinverse-investigation.md`: a `long` whose bits
NaN-tag as `SUB_INT`/`SUB_UNINIT` survives `lstore`/`lload` but
`pop_long` (`vm/src/runtime/value_stack.rs:626`) widens the int-tagged
slot and drops the high bits.

Use this as the iteration target — it's ~250× faster than the bc-math-ec
SEGV (which is the same bug manifesting as a 5-min crash). The holistic
fix needs BOTH stages bit-exact (slot→stack AND stack→consumer); patching
only `lload` SEGVs (proven this session — see the doc's "rejected fix").
Two viable approaches: parallel type-tags on the operand stack, or a
collision-free long encoding. Repro:
```sh
cd C:/craton/CratonVM/apps/_test-suites/bc-java
CP='core/build/classes/java/main;core/build/classes/java/test;core/build/resources/main;core/build/resources/test'
JUNIT='C:\Users\Victor\AppData\Local\Temp\junit-3.8.2.jar'
target/release/cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" \
  --stack-dump-on-timeout 0 -Xmx1g -cp "$CP;$JUNIT" \
  junit.textui.TestRunner org.bouncycastle.math.raw.test.AllTests
```

### Two new DaCapo crash signatures (distinct from the dispatch family)

- **`dacapo-batik` → rc=132 SIGILL** ("Illegal instruction"). The JIT is
  emitting an x86 opcode the CPU rejects. Re-run with
  `CRATONVM_DISABLE_JIT=1` to confirm it's JIT-only, then bisect with
  `CRATONVM_JIT_BISECT_ONLY` to find the offending class. This is a real
  codegen bug, NOT the OOBFIELD family.
- **`dacapo-tradebeans` / `dacapo-tradesoap` → rc=139 SEGV at ~59 s.**
  The consistent ~60 s timing across both points at a specific GC event
  (likely the second major/old-gen collection) moving an object a
  JIT frame still holds a stale pointer to — same family as #23.

### Reverted-this-session (do NOT re-attempt without the prerequisite)

- **3 JIT precision fixes** (dup oop-mark, inline-getfield L/[ mark, L/[
  putfield safepoint bracket). #3 cost ~40 % on regression-pool; #1/#2
  SEGV'd bc-math-ec via the documented `stack_oop_marks`
  may-be-shorter-than-`stack` desync. Prereq + re-application criteria in
  `docs/jit-safepoint-revert.md`. The marks/stack desync must be
  eliminated first.
- **Interpreter `lload`/`dload` bit-exact pass-through** — incomplete
  (only fixes stage 2 of 3); see the long-bit doc.

### Current BC + DaCapo snapshot

`test-infra/run-bc-dacapo-suite.sh` runs the whole battery; latest TSV is
`test-infra/suite-results/bc-dacapo-20260528-222154.tsv`.

BC core **3/9 PASS**: ✅ asn1-regression (84.9 s), crypto-prng-regression
(15.7 s), util-utiltest (2.1 s). ❌ math-raw (long-bit), crypto-regression
(clinit IllegalStateException), util-encoders (`Unexpected encoded
character`), math-ec / math / pqc-crypto (120 s timeout).

DaCapo **0/15 PASS** — all blocked on the dispatch-bypass family or the
two new crash signatures above.

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
| BC ASN.1 RegressionTest | 58/58 | 58/58 | run `main` directly — NOT a JUnit class; do NOT use `junit.textui.TestRunner` (it reports "No tests found"). 84.9 s. |
| BC crypto-prng RegressionTest | PASS | — | 15.7 s |
| BC util-utiltest (JUnit) | PASS | — | 2.1 s |

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

## Recent commits (dev tip = `55ca37f`)

```
55ca37f test-infra: BC core + DaCapo suite runner; first snapshot
99796e5 docs+diag: Tomcat NIO Selector — natives bypassed at Selector.open dispatch
e2031a2 native: RandomAccessFile.getFD mirrors fd_id into real-JDK fd/handle slots; ctor reads File.path by name
cdcf159 native: handle real-JDK InetSocketAddress holder layout in NIO bind  (Tomcat reaches "Server startup")
ac489f9 docs: capture reverted JIT precision fixes + BC EC long-bit-collision investigation
3c288b0 native: remove synthetic Connector.startInternal / AbstractProtocol.start stubs
9308959 native: RandomAccessFile real-JDK fd layout + FileDescriptor.sync0
2504a29 suite-results: 3 more JIT-corruption-family suites unblocked
19301f4 jit: skip-list TestRunner.main + CleanerImpl.run (issue #23 family expansion)
7bce079 jit: skip-list HashtableOfInt.rehash (issue #23, ECJ /by-zero)
0b5bcf5 bigint: bi_mod_str must preserve sign of dividend (modInverse negatives + large primes)
003876c compact-value: SUB_RETADDR payload bits 32-46 set → decode as Long
1cf7e96 compact-value: store longs verbatim — fixes BC SM2 F2m bit-49 corruption
```

**Investigation docs added this session (read before re-attempting):**
- `docs/tomcat-selector-investigation.md` — dispatch-bypass family + 3 fix paths
- `docs/bc-ec-mod-mododdinverse-investigation.md` — long-bit-collision 3-stage loss model
- `docs/jit-safepoint-revert.md` — reverted JIT precision fixes + re-application prereqs

Pick the highest-impact open issue you can close in your session, fix
the underlying bug (no synthetic stubs), commit & push to `dev`,
ensure the regression pool still passes.

---

# JIT correctness knowledge base — session 2026-05-29 (category-2 longs, virtual dispatch, Character.getType)

## Already FIXED and in `dev`
1. **Category-2 (long/double) interpreter NaN-tag collision** — commit `69d1401`.
   A `CompactValue` int and a long whose 47-bit payload is `< 2^32` share an
   identical bit pattern (SUB_INT sub-tag), so longs like BC safegcd's
   `0xFFFC_…` accumulators were truncated to int on verified long-consumer
   paths. Fixed by descriptor-aware decode (`int_tag_collision_long` /
   `decode_by_descriptor(b'J')` in `types/src/compact_value.rs`) across long
   loads/returns/invoke-arg-pops/OSR/putstatic. → bc-math-raw passes `--nojit`.
2. **test_unroll malformed unit-test bytecode** (`jit/src/x64.rs` tests).
   3 tests hand-encoded branch offsets that landed mid-instruction → JIT jumped
   to a bad address (crash) / compile bailed. Fixes:
   `test_unroll_with_getfield_helper_call` `if_icmpge +15→+16`;
   `test_unroll_mints_per_clone_pic_slots` same + `num_jit_args 2→1` (0-arg call
   so the following `iadd` stays balanced); `test_unroll_with_two_getfields_per_body`
   `if_icmpge +20→+21` AND `goto -20→-21`. Full `cratonvm-jit --lib`: 680/0.
3. **JIT virtual-dispatch (MIC) resolved callee by STATIC type** — `vm/src/jit/helpers.rs`,
   `jit_invoke_virtual_mic` (~lines 2319, 2412). The monomorphic inline cache
   populated `cached_entry_ptr` via `try_compile_callee(vm, info)`, using
   `info.class_name` = the *static* call-site type. For an `equals(Object)`
   call through an `Object`-typed reference (junit `assertEquals(Object,Object)`
   → `expected.equals(actual)`), it cached `Object.equals` (identity `==`) keyed
   to the `Long` receiver → two distinct equal-valued Longs compared unequal.
   Fix: resolve via the RECEIVER's `class_name`
   (`crate::runtime::interpreter::try_jit_compile_callee(vm, &class_name, info.method_name, info.descriptor)`)
   so `find_method_recursive` walks to the real override (`Long.equals`).
   `jit_invoke_dispatch` (static/special) is unchanged — `info.class_name` is
   correct there. → bc-java `InterleaveTest` 5/5 under JIT (was 2/4).
   Confirm with `CRATONVM_DBG_JIT_MIC=1` (should NOT cache `Object.equals` for
   a `Long` receiver).

## OPEN — Bug #4: `Character.getType()` returns 0 under JIT
**Symptom:** JIT-compiled `java/lang/Character.getType(int)` returns **0**
(UNASSIGNED) for valid Latin-1 letters once compiled (at the i≈2000 invocation
threshold). `Character.isLetterOrDigit` then returns false → bc-java
`Base64Test`/`UrlBase64Test`/`UTF8Test` fail with "Unexpected encoded
character X" (X is itself a valid char — `isEncodedChar` uses
`Character.isLetterOrDigit`). JIT-only; `--nojit` passes. Pre-existing
(fails on the pre-fix binary too).

**Deterministic repros** (in `C:/tmp/longbit/`): `CharTest.java`
(isLetterOrDigit false-negatives) and `CharTest2.java` (`Character.getType`
returns 0 for 'A'..'Z' from i=2000). Run under JIT:
`cratonvm.exe --java-home <jdk25> -cp C:/tmp/longbit CharTest2`.

**Mechanism (narrowed, not yet fixed):**
- `Character.getType(int)` = `CharacterData.of(cp).getType(cp)`.
  `[JIT_DISPATCH_RET/jcache] java/lang/Character.getType(I)I ret=0x0` — the
  compiled method returns 0.
- `[JIT_GEN_INVOKE_VS] pc=5 … CharacterData.getType(I)I mic_present=true pic_present=true`
  — the inner `getType` invokevirtual (abstract `CharacterData` static type)
  has a MIC/PIC site, but **no `[JIT_MIC]` helper ever fires** (set
  `CRATONVM_DBG_JIT_MIC=1`), and no `[JIT_DISPATCH]` for `CharacterData.getType`
  either → it is NOT going through the (now-fixed) MIC helper. It's resolved
  inline / devirtualized at compile time to a target that yields 0.
- `CharacterDataLatin1.getType` = `getProperties(ch) & 31`;
  `getProperties` = `A[(char)ch]` where `A` is `static final int[]`. The result
  is 0 → either the wrong `CharacterData` subclass's `getType`/`A` is selected
  (e.g. `CharacterData00`, whose table yields UNASSIGNED for Latin-1), or the
  table load is wrong.
- The MIC helper early-returns 0 *before* its dbg print on a bad receiver
  (`helpers.rs` ~2182 `receiver_raw==0`, ~2191 non-pointer guard) — consistent
  with the inner `getType` being handed a garbage/zero receiver, which would
  also explain "returns 0, no `[JIT_MIC]`".

**Ruled out** (each tested, all PASS so NOT the cause):
- General `static final int[]` load in a hot method (`StaticArr.java`).
- Abstract-base `of().getType()` pattern alone (`CD.java`).
- Non-inlinable `of()` forcing dispatch (`CD2.java`).
- Profile-driven `prepopulate` seeding — profiling is OFF by default
  (`jit::profile::enable_profiling(true)` is only called in tests;
  `PROFILING_ENABLED=false`).
- The MIC virtual-dispatch fix above (helper never fires for this site).

**Next step:** one instrument-rebuild. Add logging to the invokevirtual
codegen in `jit/src/x64.rs` (search `JIT_GEN_INVOKE_VS`, `pic_inline`,
`mic_inline`, the devirtualization/CHA path) to print how `CharacterData.getType`
is resolved for a `Latin1` receiver and which entry/target it calls. The fault
is in compile-time resolution of an `invokevirtual` whose declared type is an
abstract class with many subclasses (`CharacterData`) — it must dispatch on
the runtime receiver (`CharacterDataLatin1`), not pick a wrong subclass/abstract
target. A minimal repro likely needs the real subclass count + table layout;
iterate directly against `CharTest2` (fast to run, ~13–30 min/rebuild).

## Test-suite instructions (`apps/_test-suites/`)
Runner template: `test-infra/run-bc-dacapo-suite.sh` (paths in it are stale —
use the ones below). Binary under test should be a freshly-built
`target/release/cratonvm.exe`; HotSpot ref JDK:
`C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot`.
JUnit jar: `apps/_test-suites/junit-3.8.2.jar`.
bc-java classpath:
`BCD=apps/_test-suites/bc-java; CP="$BCD/core/build/classes/java/main;$BCD/core/build/classes/java/test;$BCD/core/build/resources/main;$BCD/core/build/resources/test;apps/_test-suites/junit-3.8.2.jar"`.
Run a JUnit suite: `cratonvm.exe --java-home <jdk> --stack-dump-on-timeout 0 -Xmx1g -cp "$CP" junit.textui.TestRunner <AllTests-class>`
(use `MSYS_NO_PATHCONV=1` under git-bash). Heap flag is `-Xmx1g` (single dash);
`--Xmx8g` (double-dash-attached) is rejected.

Suites + status (2026-05-29, JIT, correctness-first):
- `org.bouncycastle.math.raw.test.AllTests` — **PASS 4/4** (HotSpot 1.12s). Was
  failing before the category-2/virtual-dispatch fixes (`InterleaveTest`).
- `org.bouncycastle.util.utiltest.AllTests` — **PASS 18/18** (HotSpot 0.87s).
- `org.bouncycastle.util.encoders.test.AllTests` — **FAIL 13/15** → Bug #4
  (`Character.getType`); `Base64Test`, `UrlBase64Test`, `UTF8Test`. `--nojit` 15/15.
- `org.bouncycastle.asn1.test.RegressionTest` (main, no junit) — functionally OK;
  `GeneralizedTimeTest` passes standalone; only times out under CPU contention.
- Other bc suites in the runner: `bc-math-ec.AllTests` (prior 6/14 — residual EC
  `Mod.modOddInverse` int[] gaps), `bc-crypto/prng/pqc.RegressionTest`,
  `bc-math.AllTests`. commons-math (prior 3204/3204) and keycloak not re-run.

**CAVEAT:** A separate "Security audit and code review" session runs in this same
tree and can pin the CPU at 100%, inflating cratonvm timings ~2–3× and timing out
long suites. Pass/fail is still valid; treat timing as approximate unless that
session is idle. Use `git commit <pathspec>` to land work without disturbing its
uncommitted files.

---

# Keycloak test-suite errors — session 2026-05-30 (JUnit-4 crash fixed; ~84 functional failures remain)

Keycloak `core` is **JUnit 4** (43 test classes). Run directly under CratonVM via
`org.junit.runner.JUnitCore` (Surefire's junit4 fork also works once the JVM-path
rules are met — the `-Djvm` must be `<dir>/bin/java*` and the forked CratonVM
needs `CRATONVM_JAVA_HOME`; a `bin/java.bat` shim that sets it satisfies both).

Classpath: build keycloak first (already built here); deps via
`mvn -o org.apache.maven.plugins:maven-dependency-plugin:3.1.2:build-classpath
-Dmdep.includeScope=test` →
`CP = core/target/classes;core/target/test-classes;<deps>`.

## 1. (FIXED) JUnit-4 hard SIGSEGV — `newConstructorForSerialization`

Every JUnit-4 run segfaulted (exit 139) right after printing `JUnit version
4.13.2`, before any test ran — under BOTH `--nojit` and JIT (HotSpot fine).
JUnit 3 (textui) and JUnit 5 (platform) were unaffected.

Root cause: `MethodHandleNatives.init` (native-builtins/src/lang_invoke.rs) did
not populate the `type` (MethodType) field for **Constructor** MemberNames
(it did for Field; Method relies on the Java ctor). `MemberName.getMethodType()`
returns slot 2 directly, so the JDK serialization path
`ReflectionFactory.newConstructorForSerialization` →
`DirectMethodHandle.makeAllocator` → `getMethodType().changeReturnType()` /
`.returnType()` dereferenced a wild MethodType pointer → SIGSEGV.

Fix: branch **`fix/junit4-serialization-ctor`** (patch at
`C:/Projects/0001-native-invoke-populate-constructor-MemberName-Method.patch`)
— in the Constructor arm of `native_mhn_init`, build the `(parameterTypes...)void`
MethodType and store it in `type`. Minimal repro (no JUnit): `SerCtor.java` calling
`ReflectionFactory.newConstructorForSerialization(C.class).newInstance()` on a
Serializable `C`. NOT yet merged to dev (dev checkout was busy with another
agent's WIP).

### Remaining JIT-only issue exposed by the fix
Under `--nojit` JUnit 4 now works (`OK (2 tests)`). Under **JIT** the tests run but
all FAIL with `NoClassDefFoundError: Could not initialize class
java.lang.invoke.LambdaForm` via reflective `Method.invoke` (DirectMethodHandle
accessor). So `LambdaForm.<clinit>` fails under JIT — a separate, pre-existing
JIT bug. Run keycloak under `--nojit` until that is fixed.

## 2. Functional failures: CratonVM 102 vs HotSpot 18  →  ~84 CratonVM-specific

With the JUnit-4 fix (`--nojit`), keycloak core executes. HotSpot fails 18
(artifacts of the standalone JUnitCore runner missing Surefire resources — e.g.
crypto-provider registration — they fail on HotSpot too, so NOT CratonVM bugs).
CratonVM fails **102** → **~84 CratonVM-specific** failures. Spot-verified:
`JsonParserTest` HotSpot 10/10 OK vs CratonVM 7 fail; `SkeletonKeyTokenTest`
HotSpot 5/5 OK vs CratonVM 5 fail.

The 84 CratonVM-only failures, by area:

- **SD-JWT — `org.keycloak.sdjwt.*` (the large majority, ~60):**
  - `SdJwtVerificationTest` (16): `sdJwtVerificationShouldFail_*` (Duplicate
    Digest/Salt, Expired, ForbiddenClaimNames, IssuedInTheFuture, NbfInvalid,
    SdArrayElementIsNotString, InsecureHashAlg, WrongVerifier), `settingsTest`,
    `testSdJwtVerification_*` (EnforceIdempotence, FlatSdJwt, RecursiveSdJwt,
    UndisclosedArrayElements, UndisclosedNestedFields)
  - `SdJwsTest` (~14): `shouldValidateAgeSinceIssued[_IfJwtIsTooOld]`,
    `testPayloadJwsConstruction`, `testSignedJwsConstruction`,
    `testVerifyExpClaim_*`, `testVerifyIssClaim_*`, `testVerifyNotBeforeClaim_*`,
    `testVerifySignature_{Positive,WrongPublicKey}`, `testVerifyVctClaim_*`
  - `sdjwtvp.SdJwtVPVerificationTest` (~24): `testShouldFail_If*` (Kb*, Cnf*,
    DisclosureLength*, ReplayChecks*, KeyBinding*), `testShouldTolerate*`,
    `testVerif*`
  - `TimeClaimVerifierTest` (7): `testVerify{Age,Exp,Iat,NotBefore}*`
  - `DisclosureRedListTest` (7): `testDefaultRedListed*`
  - `consumer.SdJwtPresentationConsumerTest` (2), `IssuerSignedJWTTest` (1),
    `SdJwtTest.settingsTest` (1)
- **JSON parsing — `org.keycloak.JsonParserTest` (7):** `testReadClaimsParameter`,
  `testReadClientPolicy`, `testReadOIDCClientRep[WithJWKS|WithPairwise]`,
  `testResourceRepresentationParsing`, `testUnwrap` — JSON/Jackson
  deserialization round-trips diverge from HotSpot.
- **Token serialization — `org.keycloak.SkeletonKeyTokenTest` (5):** `testRSA`,
  `testSerialization`, `testToken`, `testTokenWithoutResourceAccess`,
  `testZipException`.
- **Representations (4):** `representations.IDTokenTest.testSetAddressMethodWorks`,
  `representations.UserInfoTest.testSetAddressMethodWorks`,
  `representations.workflows.WorkflowDefinitionTest.{testFullDefinition,testOnEventAsString}`.

Common threads: JSON/Jackson (de)serialization correctness and SD-JWT crypto
signature / time-claim verification (which itself leans on JSON). These are
distinct from the JUnit-4 crash and are the next functional gaps to chase.
HotSpot's own 18 (not CratonVM bugs): KeyPairVerifierTest, RSAVerifierTest,
CertificateIdentityExtractorTest, jose.{HmacTest,JWETest,jwk.*}, util.{JWKSUtils,
PemUtils}Test, sdjwt.* signing-side, JWKUtilTest.testBigInteger380bit… — all need
Surefire's resource/provider setup the standalone runner lacks.

---

# GC stale-root crash (H2 TestAll) — root cause + fix plan — session 2026-05-30

**Symptom.** `org.h2.test.TestAll` under CratonVM crashes ~20 tests in (NPE at
`TestAll.main:437`, "Cannot invoke contains on null"). Triggered by H2's
`org/h2/util/Utils.collectGarbage()` which calls `System.gc()`. The moving GC
relocates objects, then the log shows a cascade:
- `POST-GC STALE LOCAL/STACK: frame[N] <method> local/stack[M] still points to
  relocated addr 0x.. (should be 0x..)` — for `TestAll.main`/`run`/`testAll` and
  `Utils.collectGarbage` frames.
- `Stale pointer detected in invokevirtual receiver (all-zero header)` +
  `gen_heap::get_field/set_field out-of-bounds ... class_id=ClassId(0)
  java/lang/Object` (OOBFIELD family).
- `IllegalMonitorStateException` in `Utils.collectGarbage` (implicit monitorexit
  on frame pop — monitor ownership lost across the corruption).
- final `NullPointerException` → process exit.

**Where.** `vm/src/memory/gc.rs`:
- Root update pass (~lines 156-185): for each `vm.threads()` → `thread.frames`
  → each frame, forwards `frame.locals[i]` / `frame.stack[i]` when
  `slot.as_object()` is Some and its old addr is in the `forwarding` map
  (`take_forwarding_map()`).
- Post-GC verifier (~lines 187-214, gated by `CRATONVM_GC_VERIFY_STALE` or
  debug) uses the SAME iteration and flags any slot whose `as_object()` addr is
  still a forwarding KEY → emits the POST-GC STALE messages above.

**The contradiction to resolve.** Update and verify use identical
`frame.locals/stack[i].as_object()` + same forwarding map, so if the update ran
the verify could not flag it. Yet H2 flags `TestAll.main`/`Utils.collectGarbage`
frames. Leading hypotheses (verify next session):
1. **Active frame not in `thread.frames` at synchronous-GC time.** `System.gc()`
   runs inline from the executing native; the currently-executing interpreter
   frame(s) may be held in a Rust-local (or detached for speed) and not present
   in `thread.frames` during the update pass, so their roots are never forwarded;
   they reappear (stale) when pushed back, which the verifier then sees. → Fix:
   ensure the active/executing frame chain is enumerated+updated (sync the live
   frame into `thread.frames`, or update roots through the live frame too).
2. **Representation mismatch.** Object roots that live as `Value::Long`
   (tagged/JNI-style handles) or raw CompactValue slots are not matched by
   `as_object()` → never forwarded. → Fix: forward any slot whose decoded bits
   are a heap pointer in the forwarding map, not just `Value::Object`.
3. **Forwarding-map coverage.** `take_forwarding_map()` may omit some relocated
   objects (e.g. promoted/old-gen moves), so `forwarding.get(old)` misses. →
   verify the map includes every relocation.

**CAUTION — coordinate.** The actual relocation + `take_forwarding_map` live in
the **gc crate** (`gc/src/gen_heap.rs`, `heap.rs`), which a concurrent session is
mid-refactoring (uncommitted). Land the vm-side frame-root fix only against a
clean gc-crate state to avoid a cross-crate collision on core memory code.

**Repro.** Build H2 (`apps/_test-suites/h2database/h2`, `mvn -o -DskipTests
test-compile`), classpath `target/classes;target/test-classes;<test deps>`, run
`org.h2.test.TestAll` (bounded). A smaller repro: any program that holds an
object in a local across an explicit `System.gc()` that relocates it, then
dereferences it — set `CRATONVM_GC_VERIFY_STALE=1` to surface the POST-GC STALE
diagnostics.

**CONFIRMED (read of `vm/src/memory/gc.rs`, session 2026-05-30).** The post-GC
root-remap routine remaps ~16 categories — JNI globals (step 9), thread-local
refs java_thread_obj/pending_async_exception (10), root_snapshot (11), scoped
values incl. keys (12), resolution/condy cache (13), class-mirror reverse map
(14), Integer/Boolean valueOf cache (15), LambdaMetafactory CallSite cache (16)
— but it does **NOT** remap `thread.frames` interpreter **locals / operand
stack**. `verify_no_stale_refs` then iterates exactly those frame slots and emits
the POST-GC STALE LOCAL/STACK warnings — i.e. it's the canary proving the frame
remap step is missing. (Hypothesis #1/#2/#3 above superseded: the frames ARE in
`thread.frames` at remap time; the remap simply skips them.)

**Fix (vm-crate-only, no gc-crate change needed): add a frame remap step before
`verify_no_stale_refs(thread, pointer_map)`** that mirrors the verifier's loop —
for each `thread.frames` frame, for each local and each operand-stack slot, if
`Value::Object(Some(r))` and `pointer_map.get(r.as_ptr())` is Some, write the
forwarded `ObjectRef`; route other value kinds through the existing
`update_value_ref(val, pointer_map)` helper for completeness. Held pending the
concurrent gc-crate refactor only out of caution; the change itself is isolated
to `vm/src/memory/gc.rs` and uses the already-built `pointer_map`.

---

# CORRECTION + FIX: H2 GC stale-root crash — session 2026-05-30 (FIXED)

**The two earlier entries above were WRONG** about the mechanism ("missing
frame remap step"). `update_all_roots` (vm/src/memory/gc.rs) DOES remap
`thread.frames` (step 1, via `Frame::update_local_refs` +
`ValueStack::update_object_refs`). The real bug was INSIDE those two fns.

**Actual root cause.** Both update fns gated each remap on
`heap.is_object_address(old_ptr)` (frame locals) / `heap.is_heap_addr(old_ptr)`
(operand stack). Those checks read/region-test the *pre-GC* address, but the
update runs *after* evacuation when the young from-space region is reset — so a
just-moved object's OLD address fails region containment, the filter skips it,
and the live frame/stack root is left stale. `verify_no_stale_refs` (same fn,
end of `update_all_roots`) uses raw `pointer_map` membership, which is why it
flagged exactly the slots the filtered update skipped (the POST-GC STALE
LOCAL/STACK messages). Cascade: stale roots → all-zero-header `get_field` →
`IllegalMonitorStateException` in `Utils.collectGarbage` → NPE ~20 tests into
`org.h2.test.TestAll`.

**Fix (branch `fix/gc-frame-roots`, commit 7a3f0c2; vm-crate-only —
frame.rs + value_stack.rs, NOT the gc crate).** The `pointer_map` (forwarding
table) is authoritative: remap a slot iff its address is a key. The
`is_object_address`/`is_heap_addr` pre-filter never actually protected the SM2
long-bit case it was added for — a verbatim long is only rewritten if its value
equals a relocated object's old address (a map key), and post-evacuation that
address fails the region check identically for a real oop and a colliding long,
so the filter only no-op'd non-key longs (never remapped anyway). The
category-2 Double-as-oop heuristic in `update_object_refs` keeps its
`is_heap_addr` guard (honest doubles span the full 64-bit space, not explicitly
object-tagged).

**Verified.** H2 TestAll runs to the time bound (rc=124, like HotSpot) with
ZERO POST-GC STALE and zero stale-pointer/all-zero/NPE/monitor errors (was a
hard crash). SM2 canary `bc-math-raw` 4/4; BC green set
(util-encoders/util-utiltest/crypto-threshold) unregressed; unit tests
`update_local_refs_does_not_touch_long_with_pointer_shaped_bits` and the two
`t10_gc_*` pass. NOTE the synthetic "object in a local across System.gc()"
repros (GcRoot/GcRoot2) do NOT trigger it — the survivors tenure before the
GC moves them; H2 TestAll remains the reliable repro.

**Not yet merged to dev** (dev checkout had a concurrent gc-crate refactor in
flight; the fix is isolated on `fix/gc-frame-roots` to avoid collision). Merge
when the gc-crate work settles.

---

## Session 2026-06-02 — security audit + test-suite hardening (agent)

All items below are **committed on `dev`** (commits `aa6d231`, `3229e28`,
`1c9fbfc`, `b641bc7`, `da4b3df`, `eb0577e`, `023236f`, `467cb80`, `9c2b330`,
`4e1a4b4`, `452873a`, `72640c2`; soak-test tweak swept into `14cd48a`).

### DONE / VERIFIED
- **Security audit (multi-agent) + remediation.** 18 findings (V1–V18): JIT
  R10 inline-cache wild-call, SecureRandom entropy-failure→throw, JNI local-ref
  heap validation, **strict bytecode verification default for untrusted classes**,
  off-heap `Unsafe` store consolidation, GC card-buffer STW drain + quiescence
  AcqRel, G1 rset epoch-gating, SecurityManager policy routing, crash-report
  latch, etc. **Real jar-signer crypto** (ECDSA P-256/P-384 + DSA + RFC 5280
  path validation via RustCrypto) replacing the fail-closed stub — 37 jar_signer
  tests pass. Docs reconciled (W^X, unsafe inventory).
- **All 17 crate lib (unit) suites GREEN** in their default config: vm 2622,
  native-builtins 2622 (**de-flaked to 2622/0 in parallel, 3× consecutive** —
  root cause was `MockNativeContext` reusing low identities across tests; now
  globally-unique), jit 686, gc 663, classloading 480, native-io 258, types 267,
  reader 256, jfr 286, native-awt 236, native-api 138, native-collections 61,
  jit-cuda 42, jit-api 28, cuda-bridge 16.
- **Repaired** the V3 JNI regression + harness `should_panic` count, and **10
  pre-existing vm lib failures** (instrument object-size 32→40 header, roots
  orphan-heap, vec-pool stats gate, value_stack `into_inner`/`scan_object_refs`
  [a real **GC-safety** fix: was dropping live young-object roots], soak-test
  determinism). All baseline-classified vs `0744269` so only genuine regressions
  were touched.
- **Integration suites run & GREEN:** classloading, gc, reader, types,
  native-api, native-collections, native-awt, jit-cuda. **vm-cli** 67 pass / 1
  pre-existing (`main_args_delivered_in_order`, empty stdout — pre-existing on
  `0744269`).

### OPEN / TO RE-VERIFY (build lock was held by a concurrent session)
- **jit `differential_double_{sum,product}_loop_matches_host`** — pre-existing
  (fails on `0744269`): JIT double-accumulation loops return `0.0`. The
  `JIT inliner/IR-gate` fix in `e0ecdc4` (x64.rs operand-stack slot allocation)
  **likely fixes this** — re-run `cargo test -p cratonvm-jit --test differential`
  to confirm.
- **native-builtins `aot_pipeline::integration_{aot_profile,cds}_roundtrip`** —
  pre-existing **parallel-flaky** under `--workspace` (`experimental-aot`
  feature; pass in isolation). CDS/temp-file global-state race — needs a shared
  test lock / unique temp path.
- **vm integration (89 files): 349 passed / 48 failed** before a hang on
  `driver_discovered_from_jar_on_classpath` (spawns real `java`, hung holding
  the cargo lock). The 48 are dominated by WIP-JVM feature gaps (`clinit_*`,
  `io_file_*`, `io_byte_buffer_*`, `t4_7_jfr_*`) and brittle source-scanner
  tests (`t11_*_safety_comments`, `t11_3_interpreter_casts_annotated` — count
  shifts whenever source is edited). A few are in the audit zone
  (`shared_vm_ranked_accessors_*` ↔ V11 lock ordering). Re-run + baseline-classify
  once the tree is clean (was not, due to the concurrent real-JCA/real-RAF session).
