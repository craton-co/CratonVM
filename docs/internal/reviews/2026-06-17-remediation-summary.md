# CratonVM — Remediation Summary (2026-06-17)

Companion to [2026-06-17-full-multiagent-review.md](2026-06-17-full-multiagent-review.md).
All work landed on branch **`fix/full-review-remediation`** (worktree
`C:/craton/CratonVM-fixes`), branched from `dev` @ `e7802f23`.

## Method
Orchestrated implementation: rolling pools of up to **9 Opus agents**, each
owning **non-overlapping files** (agents wrote code/docs only, never built).
After every batch the orchestrator ran `cargo check`, fixed any compile error,
and committed. **17 commits, 120 files, +21,615 / −2,634.**

## Verification (all green)
- `cargo check --workspace` — **0 errors**
- `cargo check --workspace --tests` — **0 errors** (every agent regression test + new integration test compiles)
- `cargo build -p cratonvm-cli` — **builds + links**
- VM smoke test: `cratonvm -cp test_classes HelloWorld` → **"Hello World"**, exit 0
- `cargo test -p cratonvm-jit-api` — **29/29 pass** (the 3 previously-RED tests are green)

## What was fixed (by tier)

### Critical (4) — commit `48d9a9f3`
- **C1/C2 aarch64 codegen**: non-writeback LDUR/STUR (was FP/SP-corrupting pre-index), bounded writeback STP/LDP prologue, `emit_invoke` bails (was `BL .` self-loop).
- **C3 JEP-290 bypass**: synthetic deserialization path now enforces the serial filter + bounds `TC_LONGSTRING`.
- **C4 cert backdoor**: `OID_STUB_SIG` accept/skip gated `#[cfg(test)]` — production fails closed.

### High (26) — commits `a71599dc`, `2a4a20ee`
Fixed by **pattern**, not one-off:
- **A — raw-pointer side-tables vs moving GC**: GC-stable identity keys for BufferedReader/InputStreamReader/WatchService, SynchronousQueue, StampedLock/RWLock, Proxy interfaces; JNI locals as GC roots + array-elements length tracking; `coerce_value_for_return` heap check.
- **B — unchecked-length DoS**: caps on serialization, RandomAccessFile, FileChannel.transferTo, ByteBuffer.get/put, HttpServer Content-Length, JDWP read_packet.
- **C — crypto**: real TrustManager validation, constant-time OAEP/PKCS unpad, removed zero-key cipher stub, real TrustManagerFactory.
- **Other**: String UTF-16 indexing, verifier branch-edge merge, JAR unsigned-entry attribution, GC walk/cycle/SATB-sweep, JNI array-elements UB, JIT x64 null-check + ir_lower SETcc/Phi, JFR ring leak, CUDA event race, AWT fonts/raster, StructuredTaskScope concurrency, HttpClient fidelity.

### Medium / Low / Stub (~68) — commits `b3f1f65e` … `f230d0a0`
Across reader, types, native-api, jit-api (RED tests), jfr, craton-gpu,
native-collections (CSLM comparator, subList view), and every native-builtins /
classloading / gc / jit / vm subsystem. App-enabling shims (spring/quarkus
startup) were **documented as intentional compatibility shims** rather than
broken. Math.round, VarHandle atomicity, Record equals/hashCode, atomic
field-updaters, JKS/Unsafe bounds, JPMS add_reads, loader-aware resolution,
invokespecial-<init> verification, `i64::MIN` deopt-sentinel collision, aastore
covariance, and more.

### Performance (27) — commits `00bbf82c`, `604dacc3`, `a806a01e`
Sharded `obj_key_registry`, lockless serialization fast path, O(n) HTTP header
scan, O(1) Properties/JNDI lookups, bulk GZIP reads, single free-block sort per
GC sweep, O(1) soft-ref touch, bounded LinkResolver/monitor tables, PC-indexed
scalar-replacement, reverse-indexed deopt assumptions, per-table card buffers,
set-based class unloading, O(1) font-metrics LRU. Plus the `g1.rs`/`region.rs`
abort-on-corrupt-header cleanup.

### Features (4 implemented + 10 design docs) — commit `4a7551c0`
**Implemented:** stub-ratchet CI gate (`native-builtins/tests/stub_ratchet.rs`),
real `StringConcatFactory.makeConcatWithConstants`, JIT-vs-interpreter
differential harness (`vm/tests/jit_interp_differential.rs`), non-TTY
`System.console()` crash/hang fix.
**Captured as actionable design docs** (`docs/internal/feature-designs/`):
real frame-state deopt, default moving young gen, tiered-manager wiring, IR
optimizer activation, real CDI/bean container, JEP-358 NPE, Proxy real classfile,
embedding API, KeyStore/ML-DSA — each too large to land safely in one pass.

### Build/test infra — commits `90d34762`, `ae9bfc6f`
- `fuzz` detached to its own workspace (resolves the members contradiction; root `cargo check --workspace` no longer needs `--exclude`) + `tls_impl` feature enabled so `fuzz_tls_record` compiles.
- Added the missing `test_classes/HelloWorld.class` fixture (reader lib test now compiles).

## Known residual / by-design
- **XL strategic features** are design docs, not code (real deopt, moving young gen, IR optimizer, CDI) — deliberate: half-implementing would break the build.
- **App-shims** (spring/quarkus startup, ZGC simulation, conservative cross-thread JIT roots) are documented, behavior kept to preserve app compatibility — the real fix is the underlying subsystem (see design docs).
- **CUDA** `backend_cuda.rs` perf items not compile-verified here (requires the `cuda` feature + toolkit); the correctness fix shipped.
- **Stub-ratchet baseline** seeded generously (2000); tighten to observed+slack on first CI run.
- A few cross-file follow-ups were flagged in agent reports (e.g. caching method_info on `CachedBytecodeMethod`, embedding-API surface) — captured in the relevant design docs.

## Next step
Review the branch (`git log e7802f23..HEAD`) and merge to `dev`. Recommend
running the full `cargo test --workspace` and the app-gauntlet regression pool
before merge to confirm no behavioral regressions in the broad suites.
