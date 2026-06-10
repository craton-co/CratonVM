# fuzz review

## Summary

- **HIGH — `fuzz_classfile` does not compile.** `fuzz_targets/fuzz_classfile.rs:10` calls `cratonvm_reader::ClassFile::parse(data)`, but `ClassFile` has no `parse` (or any other) associated function in `reader/src/class_file.rs:35-79`. The public entry points are free functions `read_class(&[u8])` / `read_class_arc(Arc<[u8]>)` in `reader/src/class_reader.rs:58,74`. The target has been dead since it was committed — running `cargo +nightly fuzz run fuzz_classfile` errors immediately. Workspace-wide grep confirms `ClassFile::parse` is referenced nowhere outside this one file.
- **HIGH — Massive untrusted-byte coverage gap.** Only `cratonvm_reader::read_class` is fuzzed. The workspace exposes at least 25 other functions that parse externally supplied bytes with **zero** fuzz coverage: `reader::jimage::JImageReader::from_bytes` (`reader/src/jimage.rs:507`, JDK module image parser, attacker-controlled), `reader::stack_map::StackMapTable::parse` (`reader/src/stack_map.rs:126`), every ASN.1/DER/X.509 decoder in `native-builtins/src/jca/asn1.rs:205,244,274,418,480,565,598`, `native-builtins/src/x509_manager.rs:333`, `native-builtins/src/security_manager/x509.rs:136,148`, the JKS and PKCS#12 keystore loaders (`native-builtins/src/keystore.rs:200,217,346`), JBoss module XML (`native-builtins/src/jboss_module_xml.rs:109`), Undertow HTTP request head (`native-builtins/src/wildfly_undertow.rs:319`), TLS record decode (`native-builtins/src/tls_impl.rs:407`), JCA properties (`native-builtins/src/properties_sidetable.rs:447`), classpath manifest (`classloading/src/class_path.rs:463`), JFR compressed-int decoders (`jfr/src/dump.rs:181,202`), bytecode disassembly, and the JVM signature parser (`reader/src/signature.rs:394,399,404`). Every one of these is reachable from a malicious .class / network input.
- **HIGH — Oracles are panic-only.** Both targets are `let _ = …(data);`. They catch UB (ASan), aborts, and panics, but assert **no** structural property: no round-trip (`parse → encode → parse`), no `Result::Ok ⇒ all internal indices in-range`, no "well-formed input must succeed" check against the curated `test_classes/*.class` corpus. Many real reader bugs would silently return `Ok(garbage)` without panicking.
- **HIGH — No corpus, no regressions, no seeds, no `fuzz.toml`.** There is no `corpus/`, no `artifacts/`, no `seeds/`, no `cargo-fuzz` config. Every `cargo fuzz run` starts from an empty corpus — coverage growth is starting from zero on every CI invocation. Past crashes (the workspace has fixed many parser issues; `class_reader.rs:32-39` documents prior 65 535-element DoS hardening) have **not** been committed as regression inputs.
- **OSS verdict: NOT READY (HIGH blockers).** The crate is technically `publish = false` (correct) and has no `LICENSE`/`NOTICE` headers on either Rust source file. Combined with the broken target, missing OSS-Fuzz integration, missing CI hook, and missing documentation, this crate is a fuzz scaffolding stub rather than a working harness.

## 1. Code review

### Fuzz target quality

- **HIGH `fuzz_targets/fuzz_classfile.rs:10` — broken target.** `ClassFile::parse` does not exist. Replace with `cratonvm_reader::read_class(data)` (free function in `class_reader.rs:58`). This is identical to `read_class.rs:16`, so the two targets are redundant: either delete one or differentiate them (e.g. `fuzz_classfile` fuzzes raw byte parsing, `read_class` drives the API used by the classloader — the comment in `read_class.rs:1-7` already promises this distinction, but they end up calling the same code).
- **HIGH `fuzz_targets/read_class.rs:16` — no `read_class_arc` coverage.** `read_class` wraps `read_class_arc` (`class_reader.rs:58-60`), which is the zero-copy path threaded through `LazyAttribute::Raw`. The lazy-attribute decode path (the `LazyAttribute::decode(&cp)` invocation) is **completely unfuzzed**: parsing succeeds with `LazyAttribute::Raw` placeholders, attribute body parsing only happens on access. A target that calls `read_class` and then `for a in cf.attributes { a.decode(&cf.constant_pool); }` is essential. Without this, the bug surface for `attribute.rs`, `instruction.rs`, and `stack_map.rs` is untested.
- **MED `fuzz_targets/*.rs` — no SPDX headers.** Workspace convention (`reader/src/lib.rs:1-2`, `reader/src/class_reader.rs:1-2`, `class_file.rs:1-2`, etc.) is `// SPDX-License-Identifier: Apache-2.0` + `// Copyright 2024-2026 Craton Software Company` on every `.rs` file. Both fuzz targets lack both lines.
- **MED `fuzz/Cargo.toml:1-13` — workspace inheritance unused.** No `edition.workspace = true`, no `license.workspace = true`, no `repository.workspace = true`, no `authors.workspace = true`. The workspace pins `rust-version = "1.77"` (root `Cargo.toml:10`) but this crate does not inherit it — a contributor could bump the toolchain unintentionally. `libfuzzer-sys = "0.4"` is not workspace-hoisted (acceptable, sole consumer).
- **LOW `fuzz/Cargo.toml` — no `[profile.release]` override.** cargo-fuzz uses the default release profile, but the workspace root sets `lto = "fat"`, `codegen-units = 1`, `panic = "unwind"` (`Cargo.toml:162-169`). LTO **massively** slows fuzz iteration (link time per rebuild). Industry practice for fuzz harnesses is `[profile.release] lto = false, codegen-units = 16, debug = 1` — set this in the crate manifest so fuzz iteration isn't blocked behind 30-second relinks.

### Oracle quality

- **HIGH — Both targets only catch panics.** A successful parse of malformed input is **not** asserted to satisfy any invariant. Examples of differential oracles that should exist:
  - **Round-trip:** for valid `.class` corpus inputs, `read_class(input).is_ok()`.
  - **No-panic-after-decode:** `for a in cf.attributes { let _ = a.decode(&cf.constant_pool); }` should not panic even on parser-Ok-but-malformed bodies (where `read_class.rs` only validates header counts, not bodies).
  - **Index-in-range:** after `Ok(cf)`, walk `methods`, `fields`, `interfaces` — every constant-pool index must be `< cf.constant_pool.len()` and resolve to the expected tag. The reader currently builds `Arc<str>` from pool entries during parse (`class_file.rs:14-21`), but bytecode indices inside `Code` attributes are not checked until later.
  - **Bounded resource use:** the parser caps at u16 (`class_reader.rs:32-36`), but no oracle currently asserts no class < 1 MiB produces an allocation > 256 MiB (a memory-amplification oracle).
- **MED — No verifier coupling.** The JVM has a bytecode verifier (presumably in `vm/` or `classloading/`). A real-world useful fuzz target is `read_class → verify`. None exists, so verifier bugs on attacker-controlled `.class` are unfuzzed.

### Coverage gaps (untrusted-byte surfaces NOT fuzzed)

Each entry is a `pub fn` that takes a `&[u8]` / `Vec<u8>` argument and is reachable from network or attacker-controlled file input. **All are HIGH priority** per the user rule "No fuzz target for X where X is parsing untrusted bytes → HIGH":

- `reader/src/jimage.rs:507` `JImageReader::from_bytes` — JDK module image (`lib/modules`), parsed at JVM startup; an attacker swapping this file gets code execution.
- `reader/src/stack_map.rs:126` `StackMapTable::parse` — verifier input, partially driven by `read_class` but not directly fuzzed.
- `reader/src/signature.rs:394,399,404` `parse_class_signature`, `parse_method_signature`, `parse_field_signature` — generics signature parser, attacker controls every byte.
- `reader/src/field_type.rs:34,50` `FieldType::parse`, `parse_partial` + `reader/src/method_descriptor.rs:24` `MethodDescriptor::parse` — descriptor strings from constant pool.
- `classloading/src/class_path.rs:463` `parse` — classpath / manifest parsing.
- `native-builtins/src/jca/asn1.rs:205,244,274,418,480,565,598` ASN.1/DER readers (header, OID, directory string, AlgorithmIdentifier, SubjectPublicKeyInfo, Extension, Extensions).
- `native-builtins/src/jca/x500.rs:207` `decode_rdns`.
- `native-builtins/src/x509_manager.rs:333` `parse_certificate`.
- `native-builtins/src/security_manager/x509.rs:136,148` `parse_signer_dn`, `parse_cert_subject_dn` (PKCS#7 / certificate DN extraction).
- `native-builtins/src/keystore.rs:200,217,346` `load_keystore`, `load_pkcs12`, `load_jks` + matching `native-builtins/src/crypto_impl.rs:3154,3257`.
- `native-builtins/src/crypto_impl.rs:2427,2789,2811` `Asn1*::parse_der`, `parse_rsa_public_key`, `parse_ecdsa_public_key`.
- `native-builtins/src/tls_impl.rs:407` `decode_record` — TLS record on the wire.
- `native-builtins/src/wildfly_undertow.rs:319` `parse_http_request_head` — HTTP request line + headers, raw network bytes.
- `native-builtins/src/jboss_module_xml.rs:109` `parse_module_xml_bytes`.
- `native-builtins/src/properties_sidetable.rs:447` `parse_properties_pub`.
- `native-builtins/src/charset.rs:238,270` `decode_with_charset`, `decode_str_named`.
- `native-builtins/src/infinispan_local.rs:140` `from_bytes`.
- `jfr/src/dump.rs:181,202` `decode_compressed_int`, `decode_compressed_long` (JFR recording file format).
- Reader `instruction.rs` — bytecode opcode decoder, exercised via lazy attribute decode (not currently fuzzed).
- The JIT compiler (presumably in `jit/`) — does it ingest bytecode after parsing? If so it also needs a fuzz target taking valid-parsed-bytecode → compile.

That is at least **20 separate fuzz targets that should exist**. The current 2 targets (one of which is broken) cover ~5% of the attack surface.

### Stubs / unimplemented

- The two fuzz targets are themselves the entire crate. No helper modules, no shared harness. There is no `corpus_to_artifacts.rs`, no minimizer driver, no replay tool.

### Performance / harness throughput

- **MED `fuzz/Cargo.toml` — no profile override.** Workspace `Cargo.toml:162-169` pins `[profile.release] lto = "fat"`. cargo-fuzz inherits release; LTO rebuilds dominate iteration time. Add a crate-local `[profile.release] lto = false, codegen-units = 16` override.
- **MED — no `#[inline(never)]` on the oracle.** Not load-bearing today, but if oracles ever do anything beyond `let _ =`, libFuzzer prefers a target-shaped function so coverage feedback isn't smeared by inlining.
- **MED — no sanitizer flags documented.** cargo-fuzz defaults to ASan; UBSan/MSan/LeakSanitizer require `--sanitizer=…`. Nothing in the crate documents which sanitizers to run.

### CI integration

- **HIGH — no CI hook.** `grep -i fuzz .github/workflows/*` returns nothing. `ci.yml` and `cuda-bridge.yml` do not run any fuzz target. Nightly fuzz (even a 60-second smoke per target) would have caught the `ClassFile::parse` compile error before commit. There is no `cargo fuzz build` step in any workflow.

## 2. Tests

### Corpus

- **HIGH — no `corpus/` directory.** Every `cargo fuzz run` starts from random bytes. The workspace owns ~hundreds of valid `.class` files under `test_classes/`, `apps/`, `vm/tests/data/`, `classloading/tests/`, etc. Seeding the corpus with a curated subset (one of each version 45 → 69, one of each attribute type, one with `StackMapTable`, one with `LineNumberTable`, one with deeply nested generics, etc.) would dramatically accelerate coverage discovery.
- **HIGH — no regression directory.** `cargo fuzz run` re-creates artifacts/ on crash; once found and "fixed", crash inputs should be moved into `corpus/<target>/regression/` so they re-run on every fuzz session. The workspace `class_reader.rs:32-39` documents at least one historic vulnerability (u16-count OOM); no minimized reproducer is stored.

### Regressions

- **HIGH — none committed.** A fuzz crate without regressions is one toolchain bump away from re-introducing every fixed bug.

### OSS-Fuzz onboarding

- **HIGH — no OSS-Fuzz config.** The standard contract is a top-level `projects/cratonvm/` directory in `google/oss-fuzz` repo, plus a `build.sh` and `Dockerfile`. Neither exists, nor is there a stub in this crate that OSS-Fuzz could call. For a security-sensitive crate (custom JVM accepting attacker-controlled `.class`/JMOD/JAR/keystore/X.509), OSS-Fuzz is the obvious next step. The two `cargo +nightly fuzz run …` lines in the targets' doc comments are not sufficient — OSS-Fuzz needs a build script that emits compiled fuzzer binaries.

### Time-budget guidance

- **MED — undocumented.** Nothing tells a contributor how long to run a target, what coverage to expect, or how to interpret libFuzzer output. A `README.md` would normally say "≥ 24 h per target nightly, ≥ 5 min per target in CI smoke".

## 3. Documentation

### Existing

- Doc comment at `fuzz_targets/fuzz_classfile.rs:1-3` — one line, says "Run with: cargo +nightly fuzz run fuzz_classfile". Wrong, since target is broken.
- Doc comment at `fuzz_targets/read_class.rs:1-7` — five lines explaining the surface and that "the parser MUST NEVER panic on arbitrary input". Adequate but minimal.

### Missing

- **HIGH — no crate-level `README.md`.** Compare to `reader/README.md` which exists. A fuzz README should list: how to install nightly + cargo-fuzz, how to run each target, where to put seeds, how to minimize a crash, how to file a regression, what sanitizer matrix to run, the expected CI cadence, and the OSS-Fuzz status.
- **HIGH — no lib-level `//!` rustdoc.** The crate has no `lib.rs` (only `[[bin]]` entries), but a fuzz workspace usually has at least a `src/lib.rs` for shared harness code (custom mutators, structured input wrappers, oracle helpers). None exists.
- **MED — no repro instructions.** When a crash is found, what's the workflow? `cargo fuzz fmt <target> artifacts/<target>/crash-…`? `cargo fuzz tmin`? Not documented.
- **MED — no link from `SECURITY.md` / `CONTRIBUTING.md` to the fuzz crate.** A drive-by security contributor cannot discover the fuzz crate without grepping. `SECURITY.md` mentions no fuzzing program; `CONTRIBUTING.md` doesn't either. `RELEASING.md:62` is the only file in the workspace that even mentions the fuzz crate, and only to say it should stay `publish = false`.

## 4. OSS readiness

### Cargo.toml

- **`name = "cratonvm-fuzz"`** — good.
- **`version = "0.0.0"`** — does not use `version.workspace = true`. Consistent with cargo-fuzz convention (the version is meaningless for unpublished fuzz crates), but inconsistent with workspace style. Acceptable.
- **`publish = false`** — correct (matches `RELEASING.md:62` guidance).
- **`edition = "2021"`** — should be `edition.workspace = true` for consistency.
- **No `license`** — should be `license.workspace = true` so an automated SPDX scan picks up `Apache-2.0`.
- **No `authors`, `repository`, `description`** — inheriting from workspace would be one-line fixes.
- **No `[lints]` table** — the workspace defines `[workspace.lints.*]`; this crate doesn't opt in via `lints.workspace = true`. Code-lint hygiene drifts.

### License headers

- **`fuzz_targets/fuzz_classfile.rs` and `read_class.rs`** — no `SPDX-License-Identifier` header. Workspace convention is universal (every `.rs` file in `reader/`, `types/`, etc.). MED.

### NOTICE / LICENSE copies

- Workspace root has `LICENSE`, `NOTICE`, `AUTHORS`. The fuzz crate doesn't ship its own copies — since `publish = false`, not strictly required, but if any contributor copies this crate out as a standalone repro, the SPDX header on each file is the only license anchor — and there isn't one.

### Blockers for crates.io publication

- None — `publish = false` is intentional. The crate should never be published.

### OSS-Fuzz onboarding path

To onboard:
1. Add `projects/cratonvm/Dockerfile` + `build.sh` upstream to `google/oss-fuzz`.
2. `build.sh` runs `cargo +nightly fuzz build --release -s address` (and similarly for `--sanitizer=undefined`, `=memory`), copies `fuzz/target/*/release/<target>` to `$OUT/`.
3. Provide seed corpus tarballs per target — `cratonvm_fuzz_<target>_seed_corpus.zip`.
4. Email `oss-fuzz@google.com` with the integration PR + 2 maintainer addresses (primary contact + backup).
5. CI gate: weekly `cargo fuzz build --workspace` smoke in `.github/workflows/ci.yml` so a new commit cannot break the build.

**OSS verdict: NOT READY.** Crate is a scaffold; broken target plus missing surfaces makes this far from a credible fuzz harness for a security-sensitive JVM.

## Top 5 fix priorities

1. **HIGH — Fix `fuzz_classfile.rs:10`** by replacing `cratonvm_reader::ClassFile::parse(data)` with `cratonvm_reader::read_class(data)` (or delete the target and rename `read_class.rs` to be the canonical reader fuzzer). Then add `cargo fuzz build` to `.github/workflows/ci.yml` so this kind of breakage cannot recur.
2. **HIGH — Add fuzz targets for the top 8 untrusted-byte surfaces:** `JImageReader::from_bytes`, `signature::parse_{class,method,field}_signature`, `MethodDescriptor::parse`, all `native-builtins/src/jca/asn1.rs` decoders, `keystore::load_pkcs12` + `load_jks`, `x509_manager::parse_certificate`, `tls_impl::decode_record`, `wildfly_undertow::parse_http_request_head`. These are the highest-CVE-risk surfaces (PKCS#12 alone has a long CVE history; JKS too).
3. **HIGH — Build an oracle harness.** Add `src/lib.rs` (or `fuzz_targets/_common.rs`) with helpers: `assert_attributes_decode(&cf)`, `assert_indices_in_range(&cf)`, `assert_no_amplification(input.len(), allocated_bytes)`. Have every reader-side target call them after `Ok(cf)`. This catches `Ok(garbage)`, not just panics.
4. **HIGH — Seed corpora + commit regressions.** Create `corpus/read_class/` from a curated subset of `test_classes/*.class` and `apps/*/build/**/*.class`. Each historic parser bug (the u16-count DoS at `class_reader.rs:32-39`, anything in the git log under `fix.*reader`) gets a minimised crash file under `corpus/read_class/regression/`.
5. **MED — Crate hygiene + docs.** Add SPDX headers to both `.rs` files, switch `Cargo.toml` to `edition.workspace = true / license.workspace = true / authors.workspace = true / repository.workspace = true`, add `[profile.release] lto = false, codegen-units = 16, debug = 1` override so fuzz iteration is not LTO-bound, opt in to `[lints] workspace = true`, write a `README.md` documenting nightly toolchain + `cargo install cargo-fuzz` + `cargo +nightly fuzz run <target>` + `cargo fuzz tmin/fmt` workflow + the OSS-Fuzz onboarding plan above, link from `SECURITY.md` and `CONTRIBUTING.md`.
