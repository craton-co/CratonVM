# CratonVM fix orchestration tracker (branch: dev)

Pool of up to 9 parallel Opus agents per wave. Each item owns a disjoint file set.
Agents write code only (no build). Orchestrator builds + fixes + commits between waves.

Legend: ⬜ queued · 🔄 in wave · ✅ landed+built · ⏭️ deferred

## Wave 1 — CRITICAL / HIGH (code) — ✅ COMMITTED 8dcf0e3, build+tests green
- ✅ W1-awt-uaf · W1-jit · W1-coll · W1-gc(655/0) · W1-cuda-jit · W1-cuda-bridge · W1-types · W1-awt-render · W1-vm
- ✅ orchestrator fix: helpers.rs Result vs Option; jit bytecode_len_at truncated-switch OOB (completes W1-jit)

## Wave 2 — MEDIUM (code) — ✅ COMMITTED ae9be14 + follow-up; native-io 258/0, jfr 286/0
- ✅ W2-nb-panama · W2-nb-unsafe · W2-nb-asn1ser · W2-nb-misc
- ✅ W2-nio-native · W2-nio-dbb · W2-nio-ssrf · W2-nio-tests · W2-jfr
- ✅ orchestrator fix: mock alloc_object_with_class (BAIS bulk-read); jfr active_recording_count scan

## Wave 3 — MEDIUM/LOW (code) — ✅ built clean; tests green; pending commit (bundled w/ jit fix)
- ✅ W3-vmcli · W3-reader(256/0) · W3-native-api(138/0) · W3-classloading · W3-jit-api
- ✅ W3-craton-gpu · W3-numa-oldgen(gc 655/0) · W3-perf-jit · W3-coll-idhash(gc_relocation_harness 8/0)

## Pre-existing failures (NOT ours — fail on base 6165d4c; in untouched files) — TODO separate
- ❗ classloading proxy_gen::emitted_class_is_straight_line_no_handlers (proxy_gen.rs:1593 panic)
- ❗ classloading verifier::concrete_class_missing_abstract_impl_rejected (verify_class_structure too lenient)
- ❗ classloading jar_signer::rsa_verify_accepts_valid_signature (RSA PKCS#1 v1.5 verify fails)
- ℹ native-io socket_channel::nb_read_returns_eagain_zero — parallel-only flake (passes single-threaded)

## Wave 4 — DOCS + OSS metadata (mostly non-code)
- ⏭️ W4-docs-crypto  — docs/CRYPTO_STATUS.md, SECURITY.md, CHANGELOG.md (reconcile crypto/SecMgr/zip-drift)
- ⏭️ W4-docs-mode    — README.md, docs/INSTALL.md (default-mode reconcile; CLI flag table)
- ⏭️ W4-docs-release — RELEASING.md, ARCHITECTURE.md (workflow status; IR claim)
- ⏭️ W4-oss-cargo    — root Cargo.toml + member Cargo.tomls (publish flags, homepage.workspace, exclude lists)
- ⏭️ W4-cleanup      — remove stray logs; .gitignore *.log; git rm --cached continue_prompt.md
- ⏭️ DEFERRED        — crate renames (jit-cuda→cratonvm-jit-cuda etc.) — invasive, touches every dependent manifest; do as a dedicated serial step last.

## Build log
(orchestrator appends cargo build results per wave here)
