# native-builtins review

## Summary
- **HIGH — Embedded test private keys are reachable from production code.** `SSLServerSocketFactory.createServerSocket(int)` (`src/t27_tls.rs:738-744`) builds a TLS server with the test certs `SERVER_CRT_PEM` / `SERVER_KEY_PEM` baked into the release binary via `include_str!("t27_certs/server.key")` (`src/t27_tls.rs:72-80`). Any caller that doesn't hand-wire a `KeyManagerFactory` accepts TLS clients with a known, shipped-in-binary RSA private key.
- **HIGH — Subprocess execution paths have no policy / SecurityManager check.** `ProcessBuilder.start()` (`src/phases_late.rs:6614-6745`) and `Runtime.exec(*)` (`src/lang_system.rs:1029, 1086, 1093, 1107, 1116, 1137`) feed Java-supplied strings straight into `std::process::Command::new(..)`. `Runtime.exec(String)` (`src/lang_system.rs:1081`) additionally splits the command via `split_whitespace()` instead of the JDK's `StringTokenizer`, which silently mangles paths with spaces; `checkExec` from `security_manager.rs` is never consulted on either path.
- **HIGH — Object deserialization has no class-name filter and Panama FFI is open by default.** `ObjectInputStream.resolveClass` returns `Value::Object(None)` (`src/serialization.rs:1294-1295`); `ObjectInputFilter.Config.setSerialFilter` is a no-op that just allocates an int-tagged synthetic (`src/serialization.rs:1957-1965, 1909-1916`). JEP-290 enforcement is effectively absent — gadget-chain deserialization is wide open. Panama downcalls default to enabled (`src/panama.rs:38-39`), so a malicious class can call arbitrary native functions via `validated_fn_ptr` without any per-module `--enable-native-access` check.
- **MED — Massive surface (163 .rs, ~82 KLOC across the three modified files alone) with no fuzz/proptest harness.** ~2 975 `#[test]` blocks, but every `zip::ZipArchive`, `xml_parse`, JCA, deserialization, manifest, and `Inflater` path lacks property-based or adversarial-input coverage. The `zip` crate is pinned to 0.6 (`Cargo.toml:70`) while the workspace migrated to 2.x — known follow-up, but blocks publish.
- **MED — Pointer-as-key handle tables risk corruption under GC compaction.** `serialization.rs` uses `this.as_ptr() as usize` as a global HashMap key (`src/serialization.rs:1126, 1159, 1188, …` — 32 sites); `zip_real.rs`, `oos_buffers`, and `ois_buffers` follow the same pattern. The `signature.rs` doc-comment (`src/jca/signature.rs:38-43`) explicitly calls out that this caused `sign()` to return signatures over `b""` after GC; the migration to identity-hash keys was only applied to JCA. JarManifest also drops attributes past 64 silently (`src/phases_late.rs:12872-12881`) — a security-relevant property (`Implementation-Vendor`, `Sealed`, `Specification-Title`, …) can be lost.

## 1. Code review

### Bugs

| Where | Severity | Description |
|---|---|---|
| `src/lang_system.rs:1081, 1100, 1123, 1138` | MED | `Runtime.exec(String)` uses `split_whitespace()` — does not honour quoted arguments, mangles paths with spaces. JDK spec uses `StringTokenizer`. |
| `src/phases_late.rs:12872` | MED | `p98_read_jar_manifest` silently caps the manifest at 32 key-value pairs (`if attr_count * 2 + 1 < 64`). Real jars routinely exceed this (Spring Boot main manifests have 20+ entries; signed jars often 40+). |
| `src/phases_late.rs:12866-12881` | LOW | Manifest parsing uses `split_once(": ")` — does not handle continuation lines (RFC 822 folded values), `Name:` followed by empty value, or CRLF normalization. |
| `src/phases_late.rs:6664-6692` | LOW | `ProcessBuilder.start` fallback walks ArrayList via slot 0/1 indices if `elementData` / `size` names are missing — silently iterates wrong data on unmodifiable List subclasses with a different layout. |
| `src/serialization.rs:1126, 1159, 1188, …` | MED | OIS/OOS state keyed on `this.as_ptr() as usize`. The same kind of bug was already documented as causing empty-message signatures in `jca/signature.rs:38-43`; here it is unfixed. |
| `src/lib.rs:15162, 15167, …` | LOW | `unsafe_obj(args, 0).unwrap()` — if a native method is dispatched with empty args (corrupt JIT thunk), panics inside the native handler. |
| `src/phases_late.rs:6325` | LOW | `Vec::with_capacity(f.size().min(1 << 27) as usize)` — `f.size()` is the central-directory uncompressed size, not validated against actual stream. `read_to_end` will still grow unboundedly on a forged size. |
| `src/zip_real.rs:71-93` | LOW | `arg_long` silently coerces `Value::Int` to `i64`; `arg_int` truncates `i64` to `i32` — masks descriptor-mismatch bugs in registration. |
| `src/lib.rs:10646` | (modified) | `clone_lhm_overlay(this, clone_ref)` — signature change visible in current diff; check the `native-collections` side matches. |

### Vulnerabilities

| Where | Severity | Description |
|---|---|---|
| `src/t27_tls.rs:72-80, 738-744, 1071, 2228` | **HIGH** | Test PEM certs/keys embedded via `include_str!` in production binary; reachable from `javax/net/ssl/SSLServerSocketFactory.createServerSocket(I)Ljava/net/ServerSocket;`. Anyone deploying CratonVM with default `javax.net.ssl` wiring gets a TLS server using a publicly known private key. Either gate behind `#[cfg(test)]`, move under `tests/fixtures/`, or fail with `IllegalStateException` if no KMF is installed. |
| `src/phases_late.rs:6614-6745` (start) `src/lang_system.rs:1029-1140` (exec) | **HIGH** | Process spawn from Java-controlled strings with no `SecurityManager.checkExec()` consultation, no allowlist, no `current_dir` containment. A privilege-escalated Java class can spawn arbitrary host commands. |
| `src/serialization.rs:1294-1295` | **HIGH** | `resolveClass` returns `Value::Object(None)` — no JEP-290 class-name filter. |
| `src/serialization.rs:1909-1965` | **HIGH** | `ObjectInputFilter.Config.setSerialFilter` is a no-op (only allocates a 4-field synthetic). `allowFilter` / `rejectFilter` return synthetic filters whose `status` field is preset but never consulted by `ois_read_value`. JEP-290 is effectively absent. |
| `src/panama.rs:38-95` | **HIGH** | `NATIVE_ACCESS_ENABLED` defaults to `true`. `validated_fn_ptr` only checks `!= 0` and `align_of::<usize>()` — a `transmute_copy` of an attacker-supplied integer to `extern "C" fn` is gated by neither `--enable-native-access` nor a module identity (the TODO at `src/panama.rs:36-37` admits this). |
| `src/phases_late.rs:12837-12885` | **HIGH** | `p98_read_jar_manifest` does NOT verify signed-JAR signatures — no `../../../apps/META-INF/*.SF` digest check, no signature-block verification against the manifest. Combined with `Class.getSigners()` (`src/lang_class.rs:9968-9994`) returning bytes from a `CodeSource` that was never validated, downstream code that trusts `Class.getSigners() != null` is misled. |
| `src/lang_class.rs:10013-10027` | MED | `Class.setSigners` is a documented no-op that silently discards the argument. A `tracing::debug!` is the only signal — code that relies on `setSigners(...)` for security state has no way to observe the failure. |
| `src/lang_class.rs:8613, 8622` and `src/phases_late.rs:11627, 12342, 12382, 12432, 12558, 12637, 12845` (and 9 more) | MED | `zip::ZipArchive::new` invoked on attacker-controlled jar bytes with no size cap, no entry count cap, no quadratic-decompression guard — classic zip-bomb / 42.zip surface. `zip` 0.6 is the unmaintained pre-fork crate and has had two RUSTSEC advisories in 2023/2024. Workspace pinned 2.x — migrate. |
| `src/phases_late.rs:6315-6328` | MED | `jarfs_read_entry` loads the entire jar into memory (`std::fs::read(jar)?`) on every read — both a perf issue and an OOM vector for large jars or many concurrent reads. |
| `src/classloader.rs:692-697, 744-751` | LOW | The cglib short-circuit substitutes `java/lang/Object` as the class mirror, then `tracing::warn!`s. A test that relied on `cglib_proxy_name`'s short-circuit silently returns a Class of the wrong type — verification deferred to the next field/method access. |
| `src/logmanager.rs:169-171, 299, 322, 363, 375, 592, 608, 623, 647, 656, 698, 710, 1283` | MED | Stores ObjectRef raw pointers in process-wide tables. The doc claims "we never free LogManager / Logger singletons"; but the assertion is not enforced by typing and any future heap-mover would silently corrupt the registry. Migrate to identity-hash like `signature.rs`. |
| `src/serialization.rs:33-41, 45-47` | MED | Same pointer-as-key pattern in OIS/OOS handle and buffer registries. |

### Stubs / TODO inventory

No `todo!()` / `unimplemented!()` macros in this crate. The "stubs" are framework-orchestrator wiring placeholders — registration functions that exist but are *not* invoked from `lib::register_builtins`. The author left a `TODO(orchestrator)` comment in each:

| File:Line | Function (registered? Y/N) |
|---|---|
| `src/activemq_extras.rs:46` | `register_activemq_stubs` — N |
| `src/felix_extras.rs:50` | `register_felix_stubs` — N |
| `src/flink_extras.rs:37, 92` | `register_flink_stubs` — N |
| `src/grpc_extras.rs:37, 97` | `register_grpc_stubs` — N |
| `src/hazelcast_extras.rs:36, 88` | `register_hazelcast_stubs` — N |
| `src/hbase_extras.rs:39, 99` | `register_hbase_stubs` — N |
| `src/ignite_extras.rs:38, 92` | `register_ignite_stubs` — N |
| `src/jetty_extras.rs:51` | `register_jetty_stubs` — N |
| `src/spark_extras.rs:38, 92` | `register_spark_stubs` — N |

Other live TODOs:

| File:Line | Note |
|---|---|
| `src/panama.rs:36-37` | TODO wire `--enable-native-access` per-module — currently allow-all (HIGH issue above). |
| `src/panama.rs:1190` | TODO finalize the `Box<Cif>` field-3 lifecycle. |
| `src/panama_libffi.rs:640` | TODO note about Panama finalizer not wired. |
| `src/craton_gpu.rs:42, 861` | `PHASE4-CUDA-TODO` — every async GPU future is `Failed`. |
| `src/phases_early.rs:2256` | "round-7 HIGH bug 4 — weak-key cleanup": ThreadLocal cleanup is incomplete; documented memory-leak. |

### Unsafe soundness

172 `unsafe { ... }` blocks across 30 files; the bulk live in:

| File | Count | Notes |
|---|---|---|
| `src/test_utils.rs` | 62 | Test-only — `ObjectRef::from_raw`. |
| `src/panama.rs` | 31 | FFI dispatch. `validated_fn_ptr` (line 60-95) only checks non-null + alignment before `transmute_copy` — see Panama HIGH above. |
| `src/logmanager.rs` | 12 | `object_from_u64(addr)` (`src/logmanager.rs:169-171`) — sound *only* under the documented assumption that LogManager singletons are never freed. Brittle. |
| `src/panama_libffi.rs` | 6 | libffi calls — looks contained. |
| `src/quarkus_staticinit.rs` | 6 | Documented. |
| `src/lang_class.rs` | 5 | Mirror access. |
| `src/servlet.rs` | 5 | 2 production (libc `poll` / `WSAPoll`, line 1440, 1511) + 3 test (`ObjectRef::from_raw`, lines 3728, 3769, 3785). The libc / WSAPoll FFI looks correct. |

### Performance

| Where | Issue |
|---|---|
| `src/phases_late.rs:6315-6328, 11620-11663, 12342, 12382, 12432, 12558, 12637, 12845` and `src/lang_class.rs:8613, 8622` | Every `JarFile`-touching native re-opens and re-reads the entire jar via `std::fs::read` + `zip::ZipArchive::new`. No cache. Hot during boot. |
| `src/lib.rs:7210-7215` | `register_builtins` registers ~58 top-level groups; `lib.rs` alone calls `registry.register(...)` 2 751 times. Boot-time registration cost is non-trivial — opportunistic perf target. |
| `src/lang_system.rs:1029-1140` | `Runtime.exec` blocks on `command.output()` — synchronous capture of stdout/stderr; large outputs OOM. |
| `src/serialization.rs:33-41` | Per-OIS Vec<u8> stored under a global `Mutex<HashMap>` — every `readObject` contends a single mutex. |

## 2. Tests

### Coverage

- ~2 975 `#[test]` blocks across 144 src files plus `tests/aes_gcm_kat.rs`. Most modules carry their own `#[cfg(test)] mod tests`.
- No proptest, quickcheck, or `#[tokio::test]` — every test is a hand-written deterministic unit test (verified via grep).
- The most aggressively tested modules: `lang_class.rs` (127 tests), `crypto_impl.rs` (105), `tls_impl.rs` (105), `serialization.rs` (108), `lang_string.rs` (64), `crypto.rs` (61), `t27_tls.rs` (59), `panama.rs` (43). Boilerplate `*_extras.rs` framework-shim modules get only the standard 2 smoke-tests.
- Estimated line coverage in the heavily-tested modules: 70-80 %; **overall crate coverage estimate ≤ 55 %** because the long tail of unwired framework `*_extras.rs` registration functions is uncalled and untested.
- Integration test `tests/aes_gcm_kat.rs` is the only file under `tests/` — NIST CAVP vectors for AES-GCM only.

### Gaps

1. **ProcessBuilder / Runtime.exec — no negative-path tests.** Empty command, null arg, path-traversal directory (`../../etc/passwd`), allowlist enforcement (when added), command-injection via spaces (`Runtime.exec("ls; rm -rf /")` — current `split_whitespace` would just attempt `ls;` as a program, but verify).
2. **Serialization — no JEP-290 filter tests.** Construct a malicious stream that names `java.beans.XMLDecoder`, `org.apache.commons.collections.Transformer`, `org.springframework.core.io.PathResource`, and assert the filter rejects. Currently none exist because the filter is a no-op.
3. **JarManifest — no signed-jar tests.** No signature verification, so no test of the *absence* of verification either. Add a jar with a tampered manifest and assert that any caller relying on `Class.getSigners() != null` either gets verified bytes or a rejection.
4. **ZIP — no zip-bomb tests.** Add a 42.zip / quine-zip fixture and assert that `p98_read_jar_manifest`, `jarfs_read_entry`, and `JarFile.<init>` cap memory.
5. **Panama — no negative-path tests for `validated_fn_ptr`.** Should test (a) disabled-by-default behaviour once `NATIVE_ACCESS_ENABLED` is flipped, (b) misaligned addresses (e.g. `1`, `0x1001`), (c) genuinely-null address.
6. **TLS — no test asserts that production code refuses to start without an explicit KMF.** Currently the test certs *are* the production code path.
7. **Proptest** for the ISO-8601 `Duration.parse` parser (`src/lib.rs:54-247`), the `xml_parse` recursive-descent parser (`src/phases_late.rs:30058+`), the property-file escape parser (`src/properties_sidetable.rs`), and the policy file lexer (`src/security_manager/policy.rs`) — all are hand-rolled, error-prone, and untested against random input.
8. **Concurrency tests** for the global `oos_buffers` / `ois_buffers` / `inflater_table` / `deflater_table` mutexes — all single-mutex maps with no contention test.
9. **`*_extras.rs` modules** — the orchestrator wiring TODOs are unverified. Either delete the dead modules or write a smoke test that calls each `register_*_stubs` and asserts at least one native registration succeeds.

### Concrete additions

```rust
// src/serialization.rs — add to #[cfg(test)] mod tests
#[test]
fn jep290_rejects_disallowed_class() {
    // craft a TC_OBJECT stream that names `java.lang.Runtime`, install
    // a filter that rejects it, call readObject, assert RuntimeError::IOException.
}

// src/lang_system.rs — add
#[test]
fn runtime_exec_with_quoted_path_does_not_split() {
    // "C:\\Program Files\\bin\\foo.exe" arg — currently SHATTERS into 4 args.
}

// src/phases_late.rs — proptest
proptest! {
    #[test]
    fn xml_parse_never_panics(s in "\\PC*") {
        let _ = xml_parse(&s);
    }
}

// src/t27_tls.rs — gate certs behind cfg(test)
#[cfg(test)] const SERVER_KEY_PEM: &str = include_str!(...);
```

## 3. Documentation

### Existing
- Crate-level rustdoc in `src/lib.rs:4-7` is one paragraph. README (53 lines) covers scope/non-goals/usage cleanly and points at `docs/JDK_COVERAGE.md`.
- Per-module rustdoc headers are excellent in `src/serialization.rs`, `src/security_manager.rs`, `src/t27_tls.rs`, `src/zip_real.rs`, `src/jca/signature.rs`, `src/classloader.rs`, `src/panama.rs`, `src/quarkus_staticinit.rs`. These read like design docs (field layouts, registration order, byte-order conventions).
- `src/lang_class.rs` (~11 200 lines) has rustdoc on each public native including layout tables.
- SPDX header + Apache-2.0 copyright in every one of the 162 source files (verified via grep — 324 matches = 2 per file).

### Missing
1. **Security-model document.** There is no single file describing the trust posture: default-allow `SecurityManager`, no JEP-290 enforcement, Panama default-enabled, no signed-jar verification, embedded test TLS keys reachable from prod. A `SECURITY.md` or `docs/security-posture.md` is mandatory before publish.
2. **Native registration discipline.** The README mentions `register_essential_natives` + `register_builtins` but does not describe (a) the precedence rule (essential then synthetic, last-wins), (b) which class/method/descriptor triples will be overwritten by synthetic, (c) the "orchestrator" pattern of unwired `register_*_stubs`.
3. **Crate-level rustdoc** in `lib.rs:4-7` should mirror the README — currently a single throwaway sentence.
4. **Feature-flag matrix.** `Cargo.toml:13-29` defines 5+ features (`synthetic-jdk`, `deprecated-noop-tls` (no-op; was `experimental-tls`), `legacy-synthetic-crypto`, `management` (was `experimental-jmx`), `experimental-serialization`, `experimental-aot`, `gpu-offload`) — none are documented anywhere user-facing.
5. **`pub` API stability.** Functions like `cb_write_hb`, `bb_write_hb` (newly added in this PR, marked `pub(crate)`) are de-facto API for the cross-module CharBuffer/ByteBuffer plumbing — the rustdoc on `cb_write_hb` is good; `bb_write_hb` mirrors it but has terser doc.
6. **No CHANGELOG** in the crate (top-level workspace may have one, not checked).

## 4. OSS readiness

### Cargo.toml
- `name = "cratonvm-native-builtins"` — fine.
- `version.workspace = true` (0.3.0). `license.workspace = true` (Apache-2.0). `repository.workspace = true`. `description = "Java core native methods for CratonVM (java.lang.*)"` — adequate.
- `readme = "README.md"` — present.
- 28 direct dependencies — large surface. Notable:
  - **`zip = "0.6"`** — stale pre-fork crate (workspace pinned 2.x). Confirmed by Cargo.toml:70. RUSTSEC advisories in 2023-2024 affected 0.6.x. **Publish blocker.**
  - `fancy-regex = "0.18"` — fine.
  - `rustls = "0.23"`, `rustls-native-certs = "0.8"`, `native-tls = "0.2"`, `aes-gcm = "0.10"`, `ed25519-dalek = "2"`, `chacha20poly1305 = "0.10"` — all current.
  - `p12 = "0.6"` — fine but a niche crate.
  - `libffi = "3.2"` — pulls a C toolchain. Document.
- No `[badges]`, no `[package.metadata.docs.rs]`. Docs.rs build configuration absent.

### SPDX headers
- Every `.rs` file has `// SPDX-License-Identifier: Apache-2.0` and `// Copyright 2024-2026 Craton Software Company` on lines 1-2. 162 files × 2 lines = 324 matches confirmed.
- PEM files under `src/t27_certs/` carry only the PEM body, no copyright header — acceptable for test fixtures, but they should not live under `src/`.

### NOTICE / trademark exposure
- `NOTICE` (root) is 7 lines and does NOT acknowledge:
  - That this crate consumes JDK class/method/descriptor names extensively (`java/lang/*`, `javax/crypto/*`, `jdk/internal/*`). Class-name strings are not copyrightable, but a brief disclaimer is conventional (`"Java and OpenJDK are trademarks of Oracle Corporation"`).
  - The Apache-2.0 / MIT / BSD licenses of bundled third-party crates (`zip`, `flate2`, `rustls`, `aes-gcm`, `ed25519-dalek`, `libffi`, `quick-xml`, `p12`, ...).
- Compliance: low risk of trademark infringement (no claim of being "Java", README says "Java Virtual Machine implemented from scratch in Rust"), but a single sentence in NOTICE would be wise.

### Publish flag
- Workspace `publish = false` per the prompt. This crate inherits.

### Blockers for OSS publish

1. **HIGH — Embedded test TLS keys reachable from prod (`src/t27_tls.rs:72-80, 738-744`).** Gate behind `#[cfg(test)]` or move under `tests/fixtures/`.
2. **HIGH — JEP-290 deserialization filter is a no-op.** Either implement properly or document at the crate level + README "NOT SUITABLE for processing untrusted serialized streams".
3. **HIGH — Panama `--enable-native-access` defaults to enabled.** Flip the default to `false` and let host launchers opt in.
4. **MED — `zip` 0.6 migration.** Workspace already pinned 2.x; finish the upgrade.
5. **MED — README / crate docs must declare the security posture.** Default-allow `SecurityManager`, no signed-JAR verification, no JEP-290.
6. **LOW — NOTICE update.**
7. **LOW — Move `src/t27_certs/` to `tests/fixtures/` (functional reorganisation).**

## Top 5 fix priorities

1. **Remove the embedded test TLS keys from the production code path.** Gate `t27_certs/*` PEM constants behind `#[cfg(test)]` and make `SSLServerSocketFactory.createServerSocket` refuse to start if no KMF was registered. Fix at: `src/t27_tls.rs:72-80, 720-779`.
2. **Implement JEP-290 deserialization filtering for real.** Wire `ObjectInputFilter.Config.{getSerialFilter,setSerialFilter}` to a process-wide filter that `ois_read_value` consults at every `TC_OBJECT`. Replace the stub at `src/serialization.rs:1294-1295, 1909-1965`.
3. **Default-disable Panama native access and gate by per-module check.** Flip `NATIVE_ACCESS_ENABLED` default to `false`; surface the gate via `NativeContext` so launchers can grant per-module. Fix at: `src/panama.rs:36-95`.
4. **Honour `SecurityManager.checkExec` from `ProcessBuilder.start` and `Runtime.exec*`.** Call `policy_allows("java/io/FilePermission", program, "execute", ...)` before `std::process::Command::new`. Fix at: `src/phases_late.rs:6614+` and `src/lang_system.rs:1017-1140`. Also replace `split_whitespace` with a real `StringTokenizer` analogue.
5. **Migrate `zip` 0.6 → 2.x and add zip-bomb caps.** Workspace already moved. While migrating, wrap every `archive.by_name(...).read_to_end(&mut buf)` in a `Take` that enforces a global config-driven uncompressed-size limit. Fix at: `src/phases_late.rs:6315+, 11620+, 12342+, 12382+, 12432+, 12558+, 12637+, 12845+`, `src/lang_class.rs:8613-8622`, `src/jdbc.rs:150`, `src/net_phase_e.rs:91+`, `src/jboss_module_loader.rs:1629+`.
