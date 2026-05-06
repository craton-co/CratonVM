# Continue prompt — CratonVM "any Java app" path

You are continuing work on CratonVM (a Rust-based JVM at `C:\Projects\CratonVM`)
following Sessions 102-110 (commits `da5d0a4` → `32be129`). Real JDK 25 at
`C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot`.

The work pivoted from KC16-specific stubs to general JDK API gap closure. See
`docs/kc16-test-roadmap.md` for the wave-dispatch protocol and
`docs/roadmap-any-java-app.md` for the canonical surface inventory.

## First steps (your immediate batch)

Three concrete red items remain from the Yandex.Disk forcing-function run (see
`apps/scanner_probe/`, `apps/string_decode_probe/` etc. for prior probes; pattern
to follow):

### 1. Spring Boot 3.2 fat-jar loader — blocked at `ZipFile.jarStream → ensureOpen`

**Status**: PARTIAL from Session 110 (`32be129`).
S110's fix moved the failure deeper. Original NPE at
`ExecutableArchiveLauncher.getClassPathUrls:102` is gone; new blocker is
`JarFileArchive.getClassPathUrls:86 → ZipFile.jarStream:630 → ZipFile.ensureOpen:828`.

**What's needed**: implement `java.util.jar.JarFile.stream()` (returns
`Stream<JarEntry>`) AND nested-JAR support for the `BOOT-INF/lib/*.jar` layout
that Spring Boot 3.2 fat-jars use. Java's `JarFile` API supports nested JARs
via the `jar:nested:` URL scheme (Spring Boot's loader uses this).

**Reproducer**:
```bash
target/release/rustjvm.exe --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot" --Xmx 1g --jar /tmp/insurance-backend.jar 2>&1 | tail -10
# (if /tmp/insurance-backend.jar absent, copy from C:/Users/Admin/Yandex.Disk/PRO/JAVA/insurance-backend/target/insurance-0.0.1-SNAPSHOT.jar)
```

**Success criterion**: rustjvm reaches the Spring Boot banner (the ASCII-art
`Spring Boot ::                (v3.2.0)` line at minimum). Full Tomcat init
(port 8080 bound) is the stretch goal.

**Files to investigate**: `native-builtins/src/zip_real.rs` (where ZipFile is
backed), `native-builtins/src/net_url.rs` (URL.openConnection for `jar:nested:`),
`vm/tests/wave3_spring_boot_fatjar.rs` (S110's pinning test).

**Generalizes to**: every Spring Boot 3.2+ app, every "fat-jar"
distribution, every classpath-via-JAR-walking framework.

### 2. bc_probe Windows segfault — needs Windows debugger session

**Status**: PARTIAL from Sessions 108-109. Confirmed JIT codegen is the bug
(via `RUSTJVM_DISABLE_JIT=1` clearing the segfault). Targeted skip-list
extensions added (`String.toLowerCase/toUpperCase`, `Provider$ServiceKey.{hashCode,equals}`,
`Provider.{put,parseLegacy,putService,implPut}`) plus Conservative-only blanket
ban on `org/bouncycastle/*`. None sufficient under default policy.

**What's needed**: a Windows debugger (`cdb`/`WinDbg`) session that captures the
SEH `STATUS_ACCESS_VIOLATION` 0xC0000005 backtrace. The miscompiled method is
NOT yet identified — needs the actual instruction at the AV moment. This is a
"needs human at the wheel" issue; agent prompts alone have hit a ceiling
(3 agent attempts have failed to identify the exact site).

**Reproducer**:
```bash
bash apps/fetch-jars.sh   # idempotent — pulls bcprov-jdk18on-1.78.1.jar
target/release/rustjvm.exe --java-home "..." -c "apps/bc_probe;apps/bc_probe/lib/bcprov-jdk18on-1.78.1.jar" BcProbe; echo "rc=$?"
# Today: rc=-1073741819 (SEH AV)
RUSTJVM_DISABLE_JIT=1 target/release/rustjvm.exe ... # works, prints "bc.added providers=14" then hits a separate "DirectoryStream.iterator() has no Code attribute" gap
```

**Recommended approach**:
1. Run under `cdb -o target/release/rustjvm.exe ...`
2. At the AV, `kn 50` for the unmanaged stack
3. The top frame is the JIT'd code; map the RIP back to a `.compiled` entry
   via `jit/src/x64.rs::CompiledMethod::address_range`
4. The method is the candidate to add to `vm/src/jit/skip_list.rs`

**Files**: `vm/src/jit/skip_list.rs`, `vm/src/jit/x64.rs`,
`vm/tests/wave2_bc_probe.rs` (existing test pins the JIT-disabled path).

### 3. SportMe-master — needs Spring Boot dep JARs (infrastructure, not a VM bug)

**Status**: untriaged. Currently fails with `NoClassDefFoundError:
org/springframework/boot/web/servlet/support/SpringBootServletInitializer`.

**What's needed**: extend `apps/fetch-jars.sh` (S107 Cluster D v2) to download
Spring Boot starter JARs into `apps/SportMe-master/lib/` (or copy from a Maven
repo if already populated). Then re-run the app.

**Maven coordinates** (Spring Boot 2.x or 3.x — check SportMe's pom.xml):
- `org.springframework.boot:spring-boot:2.7.18`
- `org.springframework.boot:spring-boot-autoconfigure:2.7.18`
- `org.springframework.boot:spring-boot-starter:2.7.18`
- `org.springframework.boot:spring-boot-starter-web:2.7.18`
- `org.springframework:spring-context:5.3.31`
- (plus ~30 transitive deps — Spring Boot is heavy)

Realistically this needs `mvnw` or `mvn package` to do dependency resolution.
SportMe-master/pom.xml is at `C:/Users/Admin/Yandex.Disk/PRO/JAVA/SportMe-master/pom.xml`.

**Two approaches**:
- (a) Install Maven (download `apache-maven-3.9.6-bin.zip`, extract to
  `C:/Tools/maven`, add to PATH), then `cd <copy-of-sportme>; mvn package
  -DskipTests` to produce a fat-jar. **Then** test that fat-jar — likely hits
  the same Spring Boot fat-jar loader gap as item #1, so item #1 unblocks
  this anyway.
- (b) Skip — focus on item #1 instead, which subsumes this.

**Recommendation**: SKIP item 3 until item 1 lands. SportMe will then either
work or surface a new gap.

## Operational rules (carried forward from S96-S110)

- **Worktree baseline check** as step 0: `git log --oneline -1` in your
  worktree. If HEAD is older than `32be129`, run
  `git fetch origin && git merge origin/claude/intelligent-ishizaka-6d18f0 -m "merge S110 baseline"`.
- **Mandatory `RUSTJVM_STRICT_SWALLOWS=1` trace** for any `<clinit>` /
  native-resolution failure. Last 30 lines of panic backtrace verbatim in
  your report.
- **Mandatory build + repro + smoke verification** before reporting. Verbatim
  output of each step in report.
- **Tight 3-iteration timebox**. Ship the closest you got; document the gap
  in a roadmap entry.
- **Single-file scope** preferred; multi-file when registration in `lib.rs`
  is needed.
- **Restricted files** (do NOT modify):
  - `vm/src/runtime/value_stack.rs`
  - Getfield/Putfield blocks of `vm/src/runtime/interpreter.rs` (other
    parts are OK)
  - `native-builtins/src/phases_late.rs::register_phase71_natives`
- **CI gate**: `scripts/check-no-diag-prints.sh` must pass.
- **No application-specific stubs**. JDK-spec implementations only. Anti-pattern
  rejection criteria: a new file matching `*<vendor>*.rs` for vendor in
  `{jboss, wildfly, keycloak, undertow, weld, hibernate, resteasy,
   springframework, tomcat, jetty, ...}` → REJECT. (jboss-named files in
  `native-builtins/` from before S102 are tolerated; don't add new ones.)
- **Edit-tool absolute-path bug**: `Edit` with absolute `C:/Projects/CratonVM/...`
  resolves to MAIN repo, NOT your worktree. **Always edit
  `C:/Projects/CratonVM/.claude/worktrees/agent-<id>/...` paths or use
  relative paths via the working directory.** Burnt sessions on this.

## Reproducers that work today (smoke regression set)

After your fix, run these to confirm no regression:

```bash
RUSTJVM=target/release/rustjvm.exe
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.2.10-hotspot"

"$RUSTJVM" --java-home "$JDK" -c apps/bigdecimal_probe BdProbe         # 11 / 20 / OK
"$RUSTJVM" --java-home "$JDK" -c apps/atomic_probe AtomicProbe         # OK (8-thread CAS)
"$RUSTJVM" --java-home "$JDK" -c apps/dispatch_probe DispatchProbe     # InetSocketAddress.getPort()
"$RUSTJVM" --java-home "$JDK" -c apps/fjp_probe FjpProbe               # sum=499999500000 OK (RFJP.1)
"$RUSTJVM" --java-home "$JDK" -c apps/string_decode_probe StrDecode    # xs=[a, b, c]
"$RUSTJVM" --java-home "$JDK" -c apps/console_probe ConsoleProbe       # console=null / OK
printf "world\n42\n" | "$RUSTJVM" --java-home "$JDK" -c apps/scanner_probe ScannerProbe   # line=world / int=42 / OK
"$RUSTJVM" --java-home "$JDK" -c apps/selector_probe SelectorProbe     # server.recv=42 / OK
"$RUSTJVM" --java-home "$JDK" -c apps/xml_probe XmlProbe               # events=16 servers=2 OK
"$RUSTJVM" --java-home "$JDK" -c apps/methodhandles_probe MhProbe      # findStatic.jdk=42 / OK
```

Plus the two Yandex apps:
```bash
echo "5 30 0" | "$RUSTJVM" --java-home "$JDK" -c "C:/Users/Admin/AppData/Local/Temp/yandex_local/electronic-watch" com.epam.rd.autotasks.meetautocode.ElectronicWatch
# Expect: 0:00:05
echo "Alice" | "$RUSTJVM" --java-home "$JDK" -c "C:/Users/Admin/AppData/Local/Temp/yandex_local/meet-a-stranger" com.epam.rd.autotasks.meetastranger.MeetAStranger
# Expect: Hello, Alice
```

## Wave roadmap pointers (for after the 3 items above)

If you finish items 1-3 quickly, the canonical wave roadmap is in
`docs/kc16-test-roadmap.md`. Status snapshot:

- W1 (general gaps): 4/4 done (StAX, getResource consistency, Executor, JMX)
- W2 (reflection): 3/3 done (Class.getDeclared*, Method.invoke, MethodHandles.Lookup)
- W3 (NIO/networking): 4/4 partial-or-done (Socket, URL.openConnection, Selector, HttpClient queued)
- W4 (concurrency): 1/3 (atomic CAS pinned; 2 more — ReentrantLock + ConditionObject)
- W5 (crypto): 0/3 (DigestProbe/CipherProbe already pass — pin with regression tests)
- W6 (real-app smoke): continuous — Maven, Tomcat, Spring Boot 3, Cassandra, Kafka

Plus three accumulated follow-ups from agent investigations:

- **JIT regalloc clobber** in `vm/src/jit/x64.rs::patch_self_calls` (line 10266)
  and `emit_invoke_virtual` (line 9696+) — the underlying RFJP.1 cause; closed
  in S108 via `Long.valueOf` skip-list workaround but the regalloc bug itself
  is still there.
- **Windows debugger needed** for bc_probe (item 2 above).
- **JarFile.stream() + nested JARs** (item 1 above).

## Anti-pattern checklist (review before merging any agent's diff)

- [ ] Does it add a file matching `*<vendor>*.rs`? → REJECT
- [ ] Does it stub a method that only one application calls? → REJECT
- [ ] Is the fix justified by exactly one app and not generalizable? → REJECT
- [ ] Does it paper over a JDK-spec gap (swallow the error)? → REJECT
- [ ] Does it touch `value_stack.rs`, Getfield/Putfield blocks of
      `interpreter.rs`, or `register_phase71_natives`? → REJECT

JDK-spec fixes are always in scope: `java.*`, `javax.*`, `jdk.internal.*`,
`sun.*` if reachable from JDK 25 boot.

## Lessons learned (S96-S110 retrospective)

- **5 of 5 agents in S98** stalled at watchdog timeout (600s no-stream-progress).
  Mitigation: prefer `cargo build --release -p rustjvm-cli` over workspace-wide
  build; agent shouldn't sit on long commands.
- **3 of 6 agents in S100** discarded by harness; **5 of 7 agents in S98**
  empty worktrees post-restart. Cap parallel batch at ≤4 opus agents; sonnet
  for trivial cleanup.
- **Edit-tool absolute-path leakage**: agents using `Edit` with `C:/Projects/CratonVM/...`
  paths land changes in MAIN repo instead of their worktree. Fix: prompt
  must explicitly say "use relative paths from cwd, NOT absolute".
- **"Surface already works" pattern is common**: W2-A (Class.getDeclared*),
  W2-C (MethodHandles.Lookup), W4-A (Atomic CAS), enumtest's jrt-URL fix —
  4 of recent batches returned "no fix needed, just pinned with test". Try
  the surface BEFORE assuming a fix is needed.
- **One-line fixes are common**: vm_object.rs:223 (return None instead of
  Some(garbage)) closed the ImmutableList.toString cosmetic bug AND a
  category of `String + obj` failures. Cluster B's parent-walk break was
  similar. Don't underestimate small surgical fixes.
- **Side-effect closures**: RFJP.1 (open since S96) closed in S108 as a
  side-effect of CHM agent's `Long.valueOf` JIT-skip fix. Look for shared
  root causes across superficially-different bugs.

Good luck. The code base has gone from "27% JCK pass / can barely run
HelloWorld" (pre-S96) to running real-world Yandex CLI apps byte-for-byte
against HotSpot, the entire reflection / MethodHandle / NIO surface working,
and BigDecimal/BigInteger arithmetic correct. Keep the velocity.
