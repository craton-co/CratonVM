> Machine-specific / personal dev build scripts (e.g. `build-maindev*`,
> `*-isolated`, `check-cv20`, `build-kcboot*`, `verify-oom-fix`) live under
> `scripts/internal/`, which is gitignored and not part of the published repo.

Build Scripts (Windows .bat)
build-cpu.bat - Standard CPU build for cratonvm into the default target/release directory.
build-cpu-java.bat - Builds the java.exe binary alias (same as cratonvm but with java name) using the java-bin-alias feature. Used as CPU binary after EC fixes to avoid locked cratonvm.exe.
build-debug.bat - release-with-debug build (same codegen as release plus line-tables/symbols) for SEGV symbolization during debugging.
build-gpu.bat - GPU build with the gpu-driver feature into the target-gpu directory.

Benchmark Scripts
bench-4way.sh - 4-way vector-add benchmark comparing: HotSpot C2, CratonVM CPU (JIT on/off), CratonVM GPU, and TornadoVM. Reports best/mean nanoseconds and correctness.
bench-appsuite.sh - End-to-end app-suite timing running 8 probe classes (EnumTest, CipherProbe, SigProbe, CleanerProbe, LogTest, ResTest, ServiceLoaderManual, CglibProbe) under a single VM. Accepts mode: hotspot|cratonvm|cratonvm-gpu|tornado.
bench-poly-4way.sh - 4-way polynomial-eval benchmark (64 FMAs per element) comparing the same 4 platforms as bench-4way. Sized for ~1s HotSpot runtime at n=2^23.

Test/Run Scripts
app-checker.sh - Unified app-bringup harness replacing three earlier scripts. Modes: smoke (fast launcher-entry tests), functional (daemon-start + endpoint-probe), recursive (walk apps/ for all JARs with Main-Class), all (run all modes). Classifies failures (linkage, npe, resource, vm-bug, etc.).
real-run-all.sh - Runs every shimmed app with CRATONVM_<APP>_REAL=1 to disable shims and exercise real bytecode. Captures first error line for each app.
loop-run-all.sh - Runs test apps in parallel batches (4 at a time) with iteration label. Tests Spring Boot apps, probes, and big servers (kafka, wildfly, keycloak, elasticsearch).
loop-run-seq.sh - Sequential run of batch 2-4 apps (HBase, Ignite, Hazelcast, Spark, Flink, Payara, Eclipse, NetBeans, Hadoop, Mindustry, Nexus, CAS, gRPC, RabbitMQ, JDownloader, FreeMind).
run-all-apps-cuda.sh - Runs every app under CUDA-enabled cratonvm in GPU mode only (--gpu --print-gpu-decisions). Captures results into ROLLUP.md markdown table.
triage.sh - Quick triage script running every app to capture rc + first meaningful error line. Useful for quickly assessing app compatibility.
run-kc-tests.sh - Runs Keycloak JUnit test classes under CratonVM, one VM per class. Supports KC_DISABLE_JIT=1 env var. Reports PASS/FAIL/CRASH summary.

H2 Database Test Scripts
run-h2-nojit.bat - Runs H2 org.h2.test.TestAll --nojit on worktree build with 32m heap.
run-h2-testall.bat - Runs H2 org.h2.test.TestAll on isolated build (target-h2). JIT mode controlled by caller env.
run-one-test.bat - Runs a single H2 test class. Args: fully-qualified class name, extra flag (e.g. --nojit).

Utility Scripts
capture-hotspot-baseline.sh - Captures HotSpot C2 median performance for 24 benchmark kernels used by bench_hotspot_compare.rs. Generates JSON baseline file with schema_version, host, captured_at, and metrics.
capture-hotspot-baseline.ps1 - PowerShell port of the above, byte-identical JSON output for same inputs. Used on Windows for local baseline refresh.
check-no-diag-prints.sh - Forbids debug-trace eprintln!/println! markers ([WP*], [DIAG*], [TRACE*], etc.) in landed code. Excludes [cratonvm], tracing macros, test code, and env-gated diagnostics.
release-crates-dry-run.ps1 - Prints, or with -Execute runs, the crates.io package-list/package/publish-dry-run checklist in dependency order. See docs/RELEASE_READINESS.md.
jck-matrix.sh - Regenerates docs/jck-compliance.md from JavaTest-format JCK report summary.txt. Computes Pass% per API section. Supports --merge to preserve hand-curated Open-bugs column.
pgo.sh - Profile-guided optimization recipe: 1) instrumented build (profile-generate), 2) run bench suite, 3) merge profiles (llvm-profdata), 4) optimized rebuild (profile-use). Uses +sse4.2,+pclmulqdq target features.
pgo.cmd - Windows companion to pgo.sh with identical 4-phase flow.
sync-maven-java-shim.ps1 - Copies built java.exe to target/cratonvm-maven-shim/bin/ so Maven Surefire accepts -Djvm=... (requires parent dir named bin and basename starting with java).

Smoke Test Scripts (scripts/smoke/)
The smoke/ directory contains 17 focused smoke tests for specific libraries/frameworks:
common.sh - Shared helper functions for smoke tests.
ri1_specjvm_compiler.sh - SPECjvm compiler benchmark.
ri2_dacapo_avrora.sh - DaCapo Avrora benchmark.
ri3_dacapo_jython.sh - DaCapo Jython benchmark.
ri4_commons_lang.sh - Apache Commons Lang library.
ri5_jackson.sh - Jackson JSON library.
ri6_slf4j_logback.sh - SLF4J + Logback logging.
ri7_tomcat.sh - Apache Tomcat server.
ri8_jetty.sh - Eclipse Jetty server.
ri9_springboot.sh - Spring Boot framework.
ri10_hibernate_h2.sh - Hibernate ORM with H2 database.
ri11_netty_echo.sh - Netty networking framework echo test.
ri12_kotlin.sh - Kotlin language support.
ri13_scala.sh - Scala language support.
ri14_groovy.sh - Groovy language support.
ri15_quarkus.sh - Quarkus framework.
ri16_keycloak16.sh - Keycloak 16.x.
ri17_keycloak26.sh - Keycloak 26.x.
