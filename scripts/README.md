> Machine-specific / personal dev scripts (e.g. `build-maindev*`, `*-isolated`,
> `check-cv20`, `build-kcboot*`, `verify-oom-fix`, anything hardcoding a
> username or a one-off worktree path, and anything driven by the internal
> `apps/` third-party compatibility-testing corpus) live under
> `scripts/internal/`, which is gitignored and not part of the published repo.

Build Scripts (Windows .bat)
find-vcvars.bat - Shared helper: locates this machine's vcvars64.bat via `vswhere.exe` (falling back to a few common install paths). Not meant to be run directly — called by the build-*.bat scripts below so none of them hardcode one Visual Studio edition/year/install path.
build-cpu.bat - Standard CPU build for cratonvm into the default target/release directory.
build-cpu-java.bat - Builds the java.exe binary alias (same as cratonvm but with java name) using the java-bin-alias feature. Used as CPU binary after EC fixes to avoid locked cratonvm.exe.
build-debug.bat - release-with-debug build (same codegen as release plus line-tables/symbols) for SEGV symbolization during debugging.
build-gpu.bat - GPU build with the gpu-driver feature into the target-gpu directory.
build-libcratonvm.ps1 - Builds the libcratonvm C-ABI shared/static library and runs the C acceptance harnesses (embed_smoke.c = JNI Invocation API, embed_flat.c = flat cratonvm_* API, embed_helpers.c) against the produced artifacts.

Benchmark / Reproducibility Scripts
capture-hotspot-baseline.sh - Captures HotSpot C2 median performance for 24 benchmark kernels used by bench_hotspot_compare.rs. Generates JSON baseline file with schema_version, host, captured_at, and metrics.
capture-hotspot-baseline.ps1 - PowerShell port of the above, byte-identical JSON output for same inputs. Used on Windows for local baseline refresh.
pgo.sh - Profile-guided optimization recipe: 1) instrumented build (profile-generate), 2) run bench suite, 3) merge profiles (llvm-profdata), 4) optimized rebuild (profile-use). Uses +sse4.2,+pclmulqdq target features.
pgo.cmd - Windows companion to pgo.sh with identical 4-phase flow.

Test Scripts
run-kc-tests.sh - Runs a caller-supplied list of Keycloak JUnit test classes under CratonVM, one VM per class (classpath and class list are passed as arguments — no bundled Keycloak checkout required). Supports KC_DISABLE_JIT=1 env var. Reports PASS/FAIL/CRASH summary.

Utility Scripts
check-no-diag-prints.sh - Forbids debug-trace eprintln!/println! markers ([WP*], [DIAG*], [TRACE*], etc.) in landed code. Excludes [cratonvm], tracing macros, test code, and env-gated diagnostics.
release-crates-dry-run.ps1 - Prints, or with -Execute runs, the crates.io package-list/package/publish-dry-run checklist in dependency order. See docs/RELEASE_READINESS.md.
check-lcov-threshold.ps1 - Reads an LCOV report and fails if line coverage is below the requested threshold. CI uses it in advisory mode for the 85% coverage target.
jck-matrix.sh - Regenerates docs/jck-compliance.md from JavaTest-format JCK report summary.txt. Computes Pass% per API section. Supports --merge to preserve hand-curated Open-bugs column.
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
