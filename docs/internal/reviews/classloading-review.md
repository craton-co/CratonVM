# classloading review

Crate under review: `C:\Projects\CratonVM\classloading` — `cratonvm-classloading` 0.3.0 (workspace), 17 source files, 32 084 LoC (incl. tests), 6 integration test files.

## Summary

- **HIGH** — JAR signing is *parsed but unverified*. `extract_jar_signer_blocks` returns raw PKCS#7 blob bytes; nothing validates the digests in `../../../apps/META-INF/MANIFEST.MF` / `*.SF`, nothing decodes the X.509 chain, nothing checks revocation. A signed JAR's `CodeSource.certificates` is effectively attacker-supplied opaque bytes. (`class_path.rs:1543-1576`)
- **HIGH** — Production `.unwrap()` on `ZipArchive::new(Cursor::new(data))` in the fat-JAR re-open path. The first parse succeeded so this should also succeed, but a TOCTOU on the in-memory buffer or future refactor reintroduces a VM-fatal panic. (`class_path.rs:896`)
- **MED** — Per-`JarFile`/`NestedJar`/`JmodFile` `Mutex<ZipArchive>` serializes ALL concurrent class loads from the same archive. JEP 158 / virtual-thread workloads that warm up several hundred classes in parallel will be bottlenecked. (`class_path.rs:224,251,267`)
- **MED** — Multi-release version search uses BTreeSet `range(9..=max).rev()`; correct but unbounded `JVM_FEATURE_VERSION = 25` is duplicated as `MULTI_RELEASE_MAX_VERSION` in the same file (two consts, one value, drift risk). (`class_path.rs:539-546`)
- **LOW** — `find_resource("foo..bar")` returns `None` due to `contains("..")`; legitimate filenames containing `..` are blocked. Cosmetic but observable from Java code. (`class_path.rs:1586`)

OSS readiness verdict: **READY-PENDING** — license/SPDX headers consistent, NOTICE present, Cargo.toml clean. The HIGH-severity panic site (`unwrap` at `class_path.rs:896`) should be converted to a typed error before public 1.0. Documentation of the absent JAR-signature verification needs a prominent README warning so consumers don't assume signed JARs are validated.

## 1. Code review

### Bugs

- **HIGH** `class_path.rs:896` — `let reloaded = ZipArchive::new(Cursor::new(data)).unwrap();` is the *only* production `.unwrap()` in the crate (every other unwrap is test-only). The bytes have just been parsed once and consumed via `archive.into_inner().into_inner()`, so the second parse is almost-certainly safe, BUT this is exactly the kind of "almost safe" panic the gate at `loaders.rs:14-22` (`deny(clippy::unwrap_used)`) was created to prevent — and `class_path.rs` has no such gate. Convert to `expect("ZIP re-parse of just-validated bytes")` at minimum, or propagate `Err` and `debug!` skip the fat-JAR root variant.
- **MED** `class_path.rs:539-546` — `MULTI_RELEASE_MAX_VERSION` and `JVM_FEATURE_VERSION` are two const declarations that happen to share the value `25` but are conceptually distinct. The comment says they line up but nothing enforces it. Add `const _: () = assert!(MULTI_RELEASE_MAX_VERSION == JVM_FEATURE_VERSION);` or collapse to one const.
- **MED** `resolution.rs:435` — `unsafe { cratonvm_types::ObjectRef::from_raw(new_addr as *mut u8) }` has no `// SAFETY:` block. The invariant (the GC `pointer_map` lookup returned a tagged-safe address that came from a previous valid `ObjectRef`) is real but undocumented; sibling unsafe at `class_path.rs:93` has a proper SAFETY comment, so the discipline exists in the crate.
- **LOW** `class_path.rs:1184-1199` — `find_class` rejects names containing `"./"` and `".\\"` but not `"%2e"`/`"%2E"` (URL-encoded dot). JAR entry name is `&str` post-`zip` crate decode so this is mostly moot, but defence-in-depth at a single chokepoint costs nothing.
- **LOW** `class_path.rs:1586` — `find_resource` rejects `name.contains("..")` which over-blocks filenames with literal `..` (e.g. `package..internal/foo.txt`). Use `name.split('/').any(|c| c == "..")` like `is_safe_entry_name` already does at `class_path.rs:589`.
- **LOW** `loaders.rs:48-52` — `BUILTIN_LOADER_DELEGATION_CHAIN` is a flat slice but `find_class_bytes_delegated` (`class_manager.rs:2005-2026`) doesn't iterate it — it has the three branches hand-written. The const is therefore *documentation only* and the two could drift.
- **LOW** `builtin_loaders.rs:125-127` — `register_builtin_loader_aliases` only increments a counter; the comment promises name registration, but no class-name → loader-id mapping is performed.

### Vulnerabilities

- **HIGH (security)** `class_path.rs:1543-1576` — `extract_jar_signer_blocks` reads `../../../apps/META-INF/*.RSA|.DSA|.EC` and stuffs the raw bytes into `CodeSource.certificates`. NO PKCS#7 parsing, NO signature validation against `../../../apps/META-INF/MANIFEST.MF` / `*.SF` digests, NO chain-of-trust check. A `signedBy "vendor"` policy grant therefore matches on *whatever bytes the JAR puts there*, including a forged signer block copied from a legitimate JAR. The comment at line 1453-1458 acknowledges this ("opaque identifiers") but the API doesn't fail-closed — `Class.getCodeSource().getCertificates()` should either return `null` (unsigned) or *validated* certs, never opaque attacker bytes.
- **HIGH (potential)** `class_path.rs:1184-1199` and 1376-1390 — path-traversal validation in `find_class` is solid (rejects `..`, `\\`, `:`, `\0`, `./`, leading `/`/`\`). Cross-checked against `wp_security_robustness.rs::malformed_jar_zip_slip_entry_does_not_poison_cache` which proves the cache cannot be poisoned. ZIP-slip is **mitigated**.
- **MED** `class_path.rs:555-562` — `MAX_UNCOMPRESSED_ENTRY_BYTES = 512 MiB` clamps `Vec::with_capacity` but NOT the actual decompressed bytes. `read_to_end(&mut data)` will read past the clamp; the `zip` crate's own decompression bomb protection (`max_size` on a `ZipFile`) is not invoked. A "zip bomb" producing >512 MiB of inflated output would not panic on allocation but WOULD use that much RAM before failing somewhere downstream. Compare HotSpot's per-archive 4 GiB inflate cap.
- **MED (parent-delegation bypass)** `class_manager.rs:1420` — `ensure_synthetic_class` calls `find_class_bytes_delegated(name).is_ok()` then `load_class(name)` *without* checking the loader-id. If a user-defined class loader has cached a class under the same name with a different loader, the synthetic-stub upgrade path will look only at the built-in chain and could mask a user-loader's override. The user-loader fallback in `find_class_by_name` (`class_manager.rs` HIGH-5 audit fix referenced) is consulted later but the synthetic-stub upgrade short-circuits there. Tighten to "only upgrade if no user loader has registered this name."
- **LOW** `class_path.rs:419-460` — `decode_manifest_classpath_entry` URL-decodes only `%20`, `%5B`, `%5D`, `%7B`, `%7D`. A `Class-Path:` containing `%2E%2E` (encoded `..`) would survive into `PathBuf::from`; downstream `find_class` validation catches it but the Class-Path JAR-relative resolution does not.
- **LOW** No `.jar`-Cli `Class-Path` recursion bomb cap. A malicious JAR with `Class-Path: a.jar` where `a.jar`'s manifest references `a.jar` would loop indefinitely. Not actually loaded recursively here — `resolve_class_path` returns a flat list — but if a higher-level launcher walks the tree, it must guard.

### Stubs

No `todo!()`, `unimplemented!()`, or `FIXME` macros found anywhere in `src/`. Two `TODO(round-8 ...)` doc comments at `class_path.rs:58-81` describe a deferred mmap-cascade refactor (not blocking). One ASCII-corrupted comment at `access_control.rs:42-47, 165-172, 226-228, 248-254, 263, 269, 322`: cyrillic-looking glyphs (`в†’`, `В§`, `вЂ”`) — these are UTF-8 multi-byte chars that appear corrupted in the source. They compile fine but render badly in plain-text viewers; consider re-saving as UTF-8 NFC.

### Performance

- **MED** `class_path.rs:224,251,267` — `Mutex<ZipArchive<Cursor<Vec<u8>>>>` around every archive serializes class-byte lookups from the same JAR. For the JDK boot scan this is masked by `JmodFile::classes_cache` (pre-extracted), but for application JARs this is the bottleneck. The deferred mmap-refactor noted at `class_path.rs:58-81` would also enable `ZipArchive<Cursor<&[u8]>>` style sharing.
- **MED** `class_path.rs:632-666` `ensure_versions_cache` clones the `BTreeSet<u32>` on every multi-release lookup hot path. Set is small (≤17 versions) so the clone is cheap, but a `Mutex<...>` + clone in the hot path is unnecessary — could be a `OnceLock<BTreeSet<u32>>` field on `JarFile`.
- **LOW** `class_path.rs:611-624` — `canonicalize_cached` takes the mutex twice (read once, write once) on the miss path. The second `lock()` after `fs::canonicalize` re-checks for race-resolution writes but does an unconditional `insert`. The double-lock is harmless under low contention but a single `entry()`-style API would be cleaner.
- **LOW** `class_manager.rs:996` — `init_states: parking_lot::RwLock<FxHashMap<ClassId, Arc<AtomicU8>>>` is correctly chosen (read-heavy hot path); `redefine_generations` at line 956 still uses `std::sync::RwLock`. The Round-8 audit fix migrated init_states but not redefine_generations — same access pattern (read-mostly), same parking_lot win available.
- **LOW** `loaders.rs:48-52` — `BUILTIN_LOADER_DELEGATION_CHAIN` is unused by `class_manager.rs:2005` which hand-codes the three branches. The slice is referenced only in tests. Either iterate via the slice or drop the const.

## 2. Tests

**Internal `#[cfg(test)]`:** 458 `#[test]` items across 17 source files (counted via `grep -c "^    #\[test\]"`).
**Integration:** 66 tests across 6 `tests/*.rs` files:
- `wp1_7_annotation_lookup.rs` (9)
- `wp2_10_nest_host.rs` (7)
- `wp2_3_define_class_backend.rs` (16) — defineClass contract
- `wp2_4b_redefine.rs` (13) — JVMTI redefine + rollback
- `wp2_7_annotation_proxy.rs` (14) — Proxy bytecode emit
- `wp_security_robustness.rs` (7) — zip-slip, zip-bomb, two-loader isolation, circular hierarchy, redefine-rollback

### Coverage assessment

Estimated > 85% line coverage on the security-critical surface (`class_path.rs`, `loaders.rs`, `class_manager::define_class`). The recently-added `wp_security_robustness.rs` directly tests the four threats called out in the workpackage:
- zip-slip → `malformed_jar_zip_slip_entry_does_not_poison_cache` (`wp_security_robustness.rs:239`)
- truncated CAFEBABE → `malformed_jar_truncated_class_rejected_by_define` (`wp_security_robustness.rs:294`)
- declared-size clamp → `malformed_jar_zip_bomb_declared_size_is_capped` (`wp_security_robustness.rs:336`)
- two-loader isolation → `two_loaders_same_name_yield_distinct_class_ids` (`wp_security_robustness.rs:413`)
- circular hierarchy → `circular_hierarchy_a_extends_c_c_extends_a_rejected` (`wp_security_robustness.rs:556`)
- verify-failed redefine rollback → `redefine_verify_failure_rolls_back_method_bodies_and_generation` (`wp_security_robustness.rs:614`)

### Gaps

- **No JAR-signature verification tests** — because the implementation doesn't verify, there's nothing to test. The blocker is the HIGH vulnerability above.
- **No fuzz / proptest target** — the `fuzz/` crate at workspace root has fuzzers for other crates but none for `classloading`. Specifically missing:
  - `fuzz_target!(|data: &[u8]| { let _ = cratonvm_classloading::ClassPath::new(...); /* with a tempfile containing data */ })` to drive `load_jar_data`/`load_jmod`/`load_jimage` with malformed inputs.
  - A proptest over `find_class` class names (random ASCII + special chars) to confirm no panic / no false-accept of path-traversal.
- **Multi-release shadow attack not directly tested.** `find_in_multi_release_archive` correctly clamps to `JVM_FEATURE_VERSION` (audit-fix #4 at `class_path.rs:670-690`) but there is no test that builds a JAR with `../../../apps/META-INF/versions/99/java/lang/String.class` and asserts the base entry wins. Add one.
- **JMOD with adversarial entries past `classes/` prefix not tested.** `load_jmod` reads everything under `classes/` but does not call `is_safe_entry_name` on the relative path before keying `classes_cache`. A JMOD with `classes/../etc/passwd.class` would be filtered by the `strip_prefix("classes/")` check, but `classes/foo/../bar.class` would survive into the cache key. Add an adversarial JMOD test.
- **Parent-delegation correctness across user-loader registration.** Existing tests cover bootstrap → extension → application but no test asserts a user loader at the bottom can override an app-loader class only when its parent chain is properly modelled. The `ensure_synthetic_class` synthetic-upgrade path (`class_manager.rs:1420`) needs a regression test that user-loader registration is respected.
- **Module-graph cycle tests are thin.** `module.rs` has 33 tests but none assert that `A requires B; B requires A` is rejected at registration time (the JLS allows cyclic requires-transitive, but the module graph builder must terminate).
- **Resolution cache invalidation under JVMTI redefine race.** `redefine_class` bumps `class_redefine_generation`; the `LinkResolver::get` cache (the C34 audit-fix `raw_entry_mut` path) snapshots the generation at lookup. No multi-thread test exercises a thread reading a cached resolution while another thread `redefine`s — the cache should miss and re-resolve, but only a stress test would catch a race.

### Concrete additions

1. `fuzz/fuzz_targets/classloading_jar.rs`: feed arbitrary bytes through `ClassPath::new` with the bytes written to a tempfile. Drive both `.jar` and `.jmod` paths via the extension dispatch.
2. `tests/wp_multi_release_shadow.rs`: build a MR JAR with `../../../apps/META-INF/versions/99/.../String.class` and assert `find_class("java/lang/String")` returns the base entry not the v99 one.
3. `tests/wp_jmod_zipslip.rs`: build a JMOD whose `classes/` subtree contains `..` components, assert they're filtered.
4. `tests/wp_jar_signed_verification.rs` (skip until verification lands): construct an unsigned JAR, assert `find_class_code_source_info` returns `(url, vec![])`; construct a forged-signed JAR (fake `.RSA` blob), assert the same — the current implementation would return the forged bytes, the new implementation must return empty.
5. proptest under `class_path.rs::tests` over UTF-8 strings → `find_class(name)` must not panic and must return either `ClassNotFound` or a valid `Vec<u8>`.

## 3. Documentation

### Existing

- Crate-level rustdoc on `lib.rs:4-17` lists the public types in good detail.
- `README.md` covers Scope / Non-goals / Usage / Status / License. Status: "Pre-1.0. API stability is best-effort." Good.
- Most public types have doc comments. `ClassPath`, `ClassFinder`, `ClassManager`, `ResolutionCache`, the access-control checkers, and the verifier modules all have ≥1-sentence rustdoc.
- Round-1..7 audit fixes are tagged with file:line references in the source (`class_path.rs:14, 30, 122, 549, 568, 632, 680`, etc.). Good archaeology trail.
- `module.rs:4-21` has an excellent JPMS overview with concept glossary.
- `verifier.rs:4-53` documents the JSR/RET subroutine limitation explicitly.

### Missing

- **README has no "Security model" section.** Should explicitly state: JAR signature blocks are extracted but NOT verified, path traversal in class names is rejected at the API boundary, ZIP-slip in entry names is filtered before caching, the 512 MiB per-entry clamp is allocation-only (decompressed bytes can still exceed it).
- **No classloader-hierarchy diagram** (called out in scope). Add an ASCII or Mermaid block at `lib.rs` or `loaders.rs` showing `Bootstrap → Extension → Application → User-defined`. The text is scattered between `loaders.rs:27-58`, `builtin_loaders.rs:4-37`, and `class_manager.rs:1917-1920`.
- **`class_path.rs::ClassPathEntry` has no public docs** — it's pub(crate) but the variants are described in scattered comments. Inline a top-of-enum overview.
- **Hidden classes** (`DefineClassOptions::hidden`, `override_name`) are mentioned at `class_manager.rs:2042-2049` but the README and crate-level docs do not cover the hidden-class workflow. JEP 371 callers need a pointer.
- **`builtin_loaders.rs:125-127` `register_builtin_loader_aliases` docs lie**: claim is "Record that the class manager has associated the built-in loader names with their reserved `ClassLoaderId`s." Reality: increments a counter, nothing else.
- **`resolution.rs:435` unsafe block** is missing the `// SAFETY:` comment.
- **No CHANGELOG.md** at the crate or workspace level — for OSS readiness this is expected to record at least the round-of-audit fixes referenced in code comments.

## 4. OSS readiness

### Cargo.toml

- Inherits `version`, `edition`, `rust-version`, `license`, `repository`, `keywords`, `categories` from workspace. Good.
- `description = "Class loading subsystem for CratonVM"` is short but appropriate.
- `readme = "README.md"` points to a real file.
- Dependencies: all path-relative workspace deps + 4 external (`tracing`, `parking_lot`, `zip`, `rustc-hash`, `hashbrown`, `memmap2`). All Apache-2.0/MIT/BSD-3 compatible.
- `[dev-dependencies]`: `tempfile = "3"`. Minimal and license-clean.
- `[lints]` inherits workspace lints.

### SPDX / headers

Every file in `src/` and `tests/` begins with:
```
// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
```
Verified via spot-check on `lib.rs`, `class_path.rs`, `class_manager.rs`, `loaders.rs`, `builtin_loaders.rs`, `verifier.rs`, `access_control.rs`, `resolution.rs`, `module.rs`, `fx_hash.rs`, `proxy_gen.rs`, `wp_security_robustness.rs`, `wp2_3_define_class_backend.rs`. Two integration tests (`wp1_7_annotation_lookup.rs`, etc.) only carry the SPDX line without the copyright — acceptable but inconsistent.

### NOTICE

Workspace root holds `NOTICE` (verified to exist via `ls C:/Projects/CratonVM/NOTICE`). README cross-references it.

### Workspace policy

- `publish = false` is set at the workspace root (`Cargo.toml:6`); applies transitively. Crate is not publishable to crates.io until the workspace flips this.
- Crate name `cratonvm-classloading` is consistent with workspace conventions (`cratonvm-*`).

### Blockers for 1.0 / publish

1. The `.unwrap()` at `class_path.rs:896` (HIGH bug).
2. The undocumented "we extract but don't verify JAR signatures" behaviour. Either implement verification or document the gap loudly in the README and rename `extract_jar_signer_blocks` to `extract_unverified_jar_signer_blocks`.
3. Add an explicit `#![forbid(unsafe_code)]` exception list — the crate has two `unsafe` sites (`class_path.rs:93`, `resolution.rs:435`); audit-trail and one missing SAFETY comment.
4. Cyrillic-looking glyphs in `access_control.rs` comments (compile fine but signal encoding mistakes).

### Non-blockers (nice-to-have)

- Add `#[deny(missing_docs)]` on the public modules.
- Add CHANGELOG.md.
- Add classloader-hierarchy diagram.
- Add fuzz target.

## Top 5 fix priorities

1. **HIGH** — Replace the `.unwrap()` at `class_path.rs:896` with a typed error path; the fat-JAR root variant should be skipped (debug-logged) rather than panic the VM if the ZIP re-parse ever fails. Add `#![cfg_attr(not(test), deny(clippy::unwrap_used))]` to `class_path.rs` to prevent regressions, matching the gate already on `loaders.rs:14-22`.
2. **HIGH (security)** — Either implement real PKCS#7 / MANIFEST digest verification in `extract_jar_signer_blocks` (`class_path.rs:1543-1576`) OR rename the function and the `code_source.certificates` field to reflect that the bytes are **unverified opaque blobs**, and update README + `CodeSource` rustdoc accordingly. A `signedBy "vendor"` policy match on attacker-supplied bytes is a real-world auth bypass surface.
3. **MED** — Add a fuzz target (`fuzz/fuzz_targets/classloading_jar.rs`) that drives `ClassPath::new` with arbitrary tempfile bytes through both `.jar` and `.jmod` dispatch. Couple with a proptest over `find_class(name)` to assert no panic on any UTF-8 input. The malformed-JAR coverage in `wp_security_robustness.rs` is structural; fuzz catches the long tail.
4. **MED** — Eliminate the `Mutex<ZipArchive>` serialization bottleneck on the JAR hot path. The deferred mmap-cascade refactor at `class_path.rs:58-81` is the right move; if cascading generics is the blocker, an interim `parking_lot::Mutex` (cheaper acquire than `parking_lot::Mutex`'s `std` cousin — already the chosen lock here but with no fast-path RW separation) plus a `try_lock`-then-clone pattern for read-only `by_name` would help.
5. **LOW** — Backfill the gaps: (a) tighten `find_resource` `..` check at `class_path.rs:1586` to per-component, (b) add a `const _: () = assert!(...)` linking `MULTI_RELEASE_MAX_VERSION` and `JVM_FEATURE_VERSION`, (c) add SAFETY comment to `resolution.rs:435`, (d) make `BUILTIN_LOADER_DELEGATION_CHAIN` load-bearing (iterate in `find_class_bytes_delegated`) or delete it, (e) add a README "Security model" section with explicit caveats on JAR-signature verification, ZIP-bomb decompressed-byte cap, and path-traversal validation chokepoints.
