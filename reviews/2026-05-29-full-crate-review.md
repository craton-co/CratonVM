# CratonVM — Full Multi-Crate Review (2026-05-29)

Method: one Opus agent per workspace crate (18) + a docs/scripts audit + an OSS-readiness
audit, run in parallel. ~770K LOC reviewed. Findings below are de-duplicated and prioritized.

---

## 0. Headline: the workspace is not test-green

Four crates have failing unit tests on this host. This is the single most important takeaway —
fix before any release/tag.

| Crate | Failing | Root cause | Real bug? |
|---|---|---|---|
| native-collections | 1 | access-order `LinkedHashMap.put` of existing key doesn't move to tail | **Yes** |
| native-io | 5 | `range_len` whole-file lock (1, real) + stale FIS/FOS registration asserts (2) + parallel confinement-flag race (2) | 1 real, 4 harness |
| gc | 3 | zeroed reference field decodes as `Int(0)` not `Object(None)` after `Value` niche-layout change in `types` | **Yes** |
| jfr | 4 | tests share process-global ring registry; pass with `--test-threads=1` | No (harness) |

---

## 1. Code review — bugs, vulnerabilities, stubs, performance

### Critical / High severity (fix first)

1. **native-awt — Use-after-free on moving GC (HIGH).** `edt.rs:147,201-241` stores a
   `Runnable`'s bare `ObjectRef` (no GC root) in a side table when `EventQueue.invokeLater` /
   `SwingUtilities.invokeLater` is called, then dereferences it later in `InvocationEvent.dispatch`
   (`natives.rs:1882`). Because invokeLater is async, a moving collection in the window relocates/frees
   the object → UAF / type-confusion reachable from any Swing app. The sibling peer-source table was
   already hardened with a GC-generation gate (`lookup_peer_source`); the runnables table was not. Fix:
   stamp `gc_collection_count()` at register time and fail-closed, or add a real rooting/weak-handle API.

2. **jit — OOB panic on truncated bytecode (HIGH).** `x64.rs:11645` and `ir.rs:608-896` read
   `code[pc+1]/code[pc+2]` for multi-byte opcodes (bipush/sipush/iload/branches/iinc) without verifying
   operands fit in `code_len`. `jit_scan` lets a method whose final instruction is a truncated multi-byte
   op pass. The aarch64 backend guards this (`aarch64_backend.rs:3636`); x64 and IR do not. Violates the
   module's documented no-panic contract; DoS for a JIT that explicitly runs untrusted bytecode. Fix: add
   the `if pc + N >= code_len { return None }` guard already used for invokeinterface/new/anewarray.

3. **types — Fabricated heap reference from a primitive long (HIGH, latent cross-crate).**
   `compact_value.rs:737-800` `to_value()` / `is_object()` decode an attacker-controlled long whose bits
   match the NaN-box object sub-tag and form an aligned non-null address into `Value::Object(Some(..))`.
   Sound *within* this crate (degradation-counter + `*_checked` variants exist), but memory safety depends
   on every GC/interpreter/JIT caller routing reference decodes through the heap-validation guard. Audit
   all consumers; consider renaming the unchecked API to something un-ergonomic (`to_value_assume_reference`).

4. **native-collections — access-order LinkedHashMap LRU broken (HIGH, failing test).**
   `lib.rs:12327` `native_lhm_put` of an existing key overwrites the value but never moves the node to the
   tail when `lhm_is_access_order` (the get path does). Breaks the standard LRU-cache idiom. Fix mirrors
   `native_lhm_get`.

5. **gc — Zero-init reference contract violated (HIGH, failing tests).** `heap.rs:1351` `read_slot`
   does `ptr::read::<Value>` on zeroed memory; after `types` changed `Value::Object` to a `NonNull` niche,
   16 zero bytes now decode as `Int(0)` instead of `Object(None)`. `get_field_as()` masks it for `L`/`[`
   descriptors but plain `get_field()` returns the wrong variant. Fix `read_slot` to treat all-zero bits as
   `Object(None)`, or retire the contract and update tests.

6. **jit-cuda — `i2c` sign-extends instead of zero-extends (HIGH).** `emit.rs:650` routes int→char
   through the sign-extending truncate helper; Java `char` is unsigned 16-bit. `0xFFFF` becomes `-1` on GPU
   vs `65535` on CPU — silent divergence. A fixture (`EligibleI2cConvert.java`) exists but no test loads it.
   Fix: `and.b32 r, a, 0xFFFF`.

7. **cuda-bridge — `launch_on_stream` ignores the user stream (HIGH, `cuda` feature only).**
   `launch.rs:152-184` always launches on the shared `ctx.compute` stream; the `stream` arg is used only for
   event bookkeeping. Two kernels on two user streams serialize, defeating the documented per-stream
   concurrency. Acknowledged regression (HIGH-2 comment). Only manifests under the real `cuda` backend.

### Medium / notable

- **native-builtins `panama.rs:2488` `setUtf8String`** skips bounds-checking for zero-size segments
  (the read path `getUtf8String` correctly rejects them) → arbitrary native write, gated behind
  `native_access_enabled()`. Fix the asymmetry + add a negative test.
- **native-builtins `lib.rs:12433` Unsafe array `getAndSet/getAndAdd`** pass a decoded (possibly OOB)
  array index straight to the host accessor with no bounds check — OOB primitive depends on an out-of-crate
  invariant. Add an explicit `idx < len` guard.
- **native-io `nio_native.rs:378` `range_len`** under-locks whole-file `FileLock` when JDK passes
  `Long.MAX_VALUE` (failing test). **`direct_buffer.rs` ABA double-free window** when a stale `freeMemory`
  hits a recycled address (the simpler generation-counter fix was rejected — revisit). **`outbound_policy.rs`
  SSRF**: blocking connect path rechecks only built-in link-local, not custom `set_policy()` → DNS-rebinding
  bypass; `socket_channel.rs` does it correctly — make them consistent. **`read0/write0` lack a size cap**
  (net.rs has `NET_MAX_TRANSFER`) → multi-GB alloc DoS.
- **native-awt `graphics2d.rs:524`** `save()` hardcodes `composite: SrcOver` instead of snapshotting the
  current mode → `restore()` corrupts non-default compositing. Plus unbounded recursion in
  `peer.rs find_peer_at`/`destroy` (stack overflow on deep component trees).
- **jit** three raw `high - low + 1` tableswitch counts (`x64.rs:1281,1539,9731`) bypass the existing
  `checked_tableswitch_count` helper → debug-build panic on overflow. ~30 `.expect()` on patch results
  contradict the no-panic contract (clippy deny only fires under `cargo clippy`, not `cargo build`).
- **vm `jit/helpers.rs:619` `jit_newarray`** uses non-fallible `alloc_array` where the interpreter uses
  `try_alloc_array` → no catchable `OutOfMemoryError` on the JIT path; `.unwrap_or(0)` probe size diverges
  from real allocation size.
- **jfr `dump.rs` delta-timestamp underflow** clamps to 0 when an event predates chunk start (fidelity bug).
- **vm-cli `main.rs:962` split `-Xms 512m`** corrupts main-class selection (`-Xms` missing from
  `VALUE_TAKING_OPTS`); watchdog can `abort()` a cleanly-finished run on tight `--stack-dump-on-timeout`.

### Low / hardening (selected)

- native-collections 32-bit identity-hash side-table aliasing (two live collections can collide and corrupt
  each other ~50% around 77k live collections) + unbounded global-table growth (leaks an entry per
  LL/LHM/TM/TS ever created). LinkedList `ListIterator.remove()/add()` are silent no-ops (should mutate or
  throw `UnsupportedOperationException`).
- reader: deprecated panicking `ByteView::new` still `pub`; `instruction.rs` unchecked `pc+n` (inconsistent
  with hardened `buffer.rs`).
- native-api: file/socket openers take raw Java-controlled paths with no sandbox/path-traversal contract
  (boundary crate — but undocumented). `align_up` overflow.
- classloading: no-op `rest.replace('%', "%")` percent-decode at `class_path.rs:441`; lenient verifier
  accepts Java7+ branch targets lacking a StackMapTable frame (documented tradeoff).
- gc: `numa.rs parse_cpulist` can allocate ~4B entries on a malformed cpulist; 2 `unused_unsafe` warnings.
- cuda-bridge: `intern_kernel_name` `Box::leak` is unbounded; several dead `cuda`-only entry points
  (`device_count`, `optimal_block_size`, `launch_raw_no_d2h_sync`) reference nonexistent callers.

### Stubs / unimplemented (intentional unless noted)

- native-io: `epoll_wait_native`, Windows `poll0`, `readv0/writev0` return 0/unsupported (JDK uses
  `select0`); `FileInputStream.length0/position0` hardcoded to 0.
- native-builtins: 7 framework `register_*_stubs` not wired into registration; direct-ByteBuffer inflate
  paths are no-ops; JEP-290 `maxbytes/maxarray` not fully plumbed; per-module native-access gate is a
  process-wide bool.
- native-awt: entire `platform/` backend tree (win32/x11/cocoa) compiles + unit-tests but is **not
  instantiated** from natives — `Frame.setVisible(true)` opens no window. Clipboard/file-dialog unimplemented.
- jit: `loop_analysis` is analysis-only (no hoisting wired); LICM records computed but unused.
- classloading: ECDSA/DSA JAR-signature verification not implemented (fail-closed `Unsupported`).
- jit-cuda: non-static `this.field` admitted by analyzer but rejected by emitter (no `getfield` arm).
- gc: ZGC backend (`zgc.rs` ~3.1k LOC) is a feature-gated stub; NUMA multi-arena is design-intent only.
- jit-api: `gpu-lowering` feature has zero consumers (round-9 TODO to delete it).

### Performance opportunities (highest-value)

- gc `arena.rs reset` zero-fills the whole young from-space every swap — bounding `is_object_address` to the
  live cursor unlocks `reset_no_zero`, a measurable young-GC win. Per-class reference bitmaps would let the
  Cheney scan + old-gen ref-update skip primitive slots.
- jit: single global `Mutex<JitCodeRegion>` taken on every call (`validate_code_ptr`); `find_oop_map_for_pc`
  re-scans sortedness on every GC lookup; `bytecode_len_at` re-walked by every analysis pass.
- vm: methods with >4 args bail off the JIT fast path on every call; one fresh OS thread per virtual-thread
  mount (no carrier pool); conservative root scanner rescans up to 8 MiB/frame and pins false positives.
- native-collections: two `resolve_field_index` string lookups per ArrayList element op; per-element
  get/set shift loops on insert/remove (no bulk array copy); global per-family Mutex serializes all
  instances.
- native-builtins: per-byte `get/set_array_element` virtual calls on the zip inflate/deflate hot path — a
  bulk `get/set_array_region` host API would be a large win.
- reader: dual dispatch in `decode_attribute_body` (Arc::ptr_eq chain + string match).

---

## 2. Tests review — adequacy & coverage vs the 85% bar

| Crate | Est. coverage | ≥85%? | Notes |
|---|---|---|---|
| reader | 85–90% | ✅ | Strong adversarial parser tests; add a cargo-fuzz target over `read_class`. |
| types | 90–95% | ✅ | 270 tests + proptest + 16-thread intern stress. |
| native-api | 70–78% | ❌ | Socket/UDP/TLS/pipe/RandomAccessFile paths in `fd_table.rs` almost untested. |
| native-collections | 20–30% | ❌ | Streams/Collectors/Optional/Random/Properties (~thousands of LOC) untested; 1 failing test. |
| native-io | 60–70% | ❌ | No zip-bomb-guard test, no direct-buffer double-free test; 5 failing tests. |
| native-builtins | 70–80% | ❌ | 18 large files (lang_misc, xml_stax, classloader_real, craton_gpu…) have zero inline tests. |
| native-awt | ~70–78% whole / >85% core | ❌ overall | 3 platform backends (~3,150 LOC) untested at runtime; no UAF regression test. |
| jit-api | 95%+ | ✅ | ABI golden-offset + null-sweep tests; gpu_lowering untested. |
| jit | 75–85% | ⚠️ borderline | No truncated/overflow-bytecode tests on x64/IR (where the HIGH bug lives). |
| jit-cuda | 70–80% | ❌ | i2c bug uncovered; byte/char/short array ops, div/rem guards, truncation paths untested. |
| cuda-bridge | <50% whole | ❌ | Entire real `cuda` backend (the unsafe FFI) uncovered; stub tests early-return on non-GPU hosts. |
| classloading | 80–88% | ⚠️ | RSA modpow/JKS/PKCS#12 parsing lightly tested; lenient-vs-strict verifier divergence untested. |
| craton-gpu | ~0% | ❌ | Build-only crate; refactor `build.rs` pure logic to be testable. |
| gc | 80–88% | ⚠️ | Excellent (proptest + loom + soak); 3 failing tests; ZGC dark in default CI. |
| vm | 70–85% | ❌ in plain `cargo test` | Richest suites (differential vs HotSpot, JCK/TCK, real-jar bootstraps) are `#[ignore]`d and need a host JDK + staged apps that aren't present. |
| vm-cli | 70–80% | ❌ | 520-line uncaught-exception renderer + watchdog + non-.jar staging untested. |
| jfr | 88–92% | ✅ | 4 isolation failures under parallel runner; no Miri/loom for the SPSC unsafe. |
| fuzz | n/a | n/a | Harness-only; 4 of 10 targets missing from README + OSS-Fuzz build.sh; no seed corpus committed. |

**Biggest test gaps to close:** native-collections (Streams/Collectors), native-api (sockets/RAF),
native-io (zip-bomb + double-free), the JIT truncated-bytecode battery, and a CI lane that provisions a JDK
+ stages apps so the VM's `#[ignore]`d differential/JCK suites actually run.

---

## 3. Documentation & scripts review

**Strengths:** unusually complete OSS health set (GOVERNANCE, SECURITY, RELEASING, MAINTAINERS, CoC,
SUPPORT, CITATION, per-crate READMEs). Crate-count references (17 members + fuzz) are mostly consistent.

**Cross-doc contradictions to fix:**
- **Crypto status:** `SECURITY.md`/`CHANGELOG` say ECDSA-P256 implemented & RSA-PSS NotImplemented, but
  `docs/CRYPTO_STATUS.md` says the opposite on both. Reconcile against `classloading/jar_signer.rs` (which
  reports ECDSA `Unsupported`).
- **Default mode:** `README` markets "no JDK / synthetic", `INSTALL.md` says real-JDK boot is the default
  when a JDK is present. Code (`detect_real_jdk`) sides with INSTALL. README is misleading.
- **Security Manager:** `SECURITY.md` lists it as absent; `CHANGELOG` + `native-builtins/security_manager.rs`
  wire `checkExec`.
- **RELEASING.md** is self-contradictory (claims release workflow disabled, but
  `.github/workflows/release.yml` is active) and stale.
- `CHANGELOG` "Unreleased > Known follow-ups" still lists the zip-version drift that was resolved in round-12.
- `ARCHITECTURE.md` says JIT has "no IR" but README/CHANGELOG describe a sea-of-nodes IR pipeline.

**Stale paths:** `scripts/triage.sh` and `continue_prompt.md` reference `C:/craton/CratonVM` + branch `dev`;
actual repo is `C:/Projects/cratonvm` on `main`. JDK version drift (docs say 25.0.1, host is 25.0.2).

**Missing:** consolidated CLI-flag reference (`--synthetic-jdk`, `-Xverify:none`, `-Xbootclasspath/a` live
only in `main.rs`/INSTALL); automated SBOM/cargo-deny/license gate; a security-contact PGP key; an actual
dated release cut (the Unreleased block grows unbounded).

**Should remove / clean:** stray root logs (`relfix.log`, `vmcheck.log`, `vmcheck2.log` — not in
`.gitignore`); untracked `test-infra/_cratonvm-jvm/`; prune the most ephemeral `docs/internal/` session logs
before public release.

**Scripts:** Windows-primary repo but 16/18 `scripts/` + all smoke scripts are bash-only (need WSL/Git-Bash,
undocumented in BUILD_GUIDE). `triage.sh` hardcodes the wrong absolute path and uses fragile `/c`-mount
classpath munging. `check-no-diag-prints.sh` is good but not wired as a CI gate.

---

## 4. OSS / crates.io readiness (Apache-2.0, Craton Software Company)

**Legally in good shape:** LICENSE is verbatim Apache-2.0 with correct
`Copyright 2024-2026 Craton Software Company`; NOTICE satisfies §4(d); AUTHORS/CITATION/TRADEMARKS consistent;
no GPL/AGPL/copyleft deps; SPDX headers on 537/567 (94%) files. Branding uniform on `github.com/craton-co`.

**Hard blockers to crates.io publish:**
1. `publish = false` in `[workspace.package]` is inherited by all 17 members — must be flipped per shippable
   crate (keep `fuzz` unpublished).
2. **`continue_prompt.md` is committed/tracked** despite `.gitignore` listing it and a comment saying it
   "must NOT ship in the public Apache-2.0 repo" — `git rm --cached` and purge from history.
3. `docs/internal/` (76 tracked files incl. session-handoff notes + `.patch` files) was intentionally
   un-ignored — scrub before public.
4. No crate sets `exclude` → `cargo package` would bundle ~427 compiled `.class` fixtures (vm/tests,
   classloading/tests). Add `exclude` lists.
5. `homepage` is defined in `[workspace.package]` but no member adds `homepage.workspace = true` → published
   crates get an empty homepage.
6. Dependency order: `version = "0.3.0"` path-deps require publishing bottom-up (types/reader → … → vm → vm-cli).

**Naming:** three members break the `cratonvm-` prefix — `jit-cuda`, `cuda-bridge`, `craton-gpu`. Rename to
`cratonvm-*` and reserve the names on crates.io early to avoid squatting/collisions.

**Secrets:** committed TLS private keys in `native-builtins/src/t27_certs/*.key` are **test-only**
(`include_str!` inside `#[cfg(test)]`, documented) — low risk, but add a README/SECURITY note since GitHub
secret-scanning will flag them. No API keys/tokens/keystores found.

**GitHub hygiene:** CODEOWNERS, FUNDING, dependabot, PR + issue templates, and a full workflow set all
present. Verify the `@craton-co/cratonvm-maintainers` team/org actually exists. `craton.com.ar` appears
nowhere — if it's the canonical site, use it for `homepage`.

---

## 5. Suggested features & directions

**Robustness / security (do first — they harden the untrusted-bytecode boundary):**
- A standing **cargo-fuzz + OSS-Fuzz** deployment (the harness exists in `fuzz/` — just wire all 10 targets,
  commit a seed corpus, and add a CI lane). This is the highest-leverage investment for a JVM.
- A **GC rooting / weak-handle API** in `NativeContext` so native side-tables (AWT runnables, collection
  overlays) stop storing bare `ObjectRef`s across GC boundaries — kills a whole class of UAF.
- **Miri/loom CI lanes** over the documented-unsafe SPSC ring (jfr) and SATB marking (gc).
- Strict-verifier-by-default for application classloaders (lenient for platform jars).

**Performance:**
- Precise oop maps (replace conservative JIT stack scanning) + per-class reference bitmaps for GC.
- Wire the already-computed LICM/loop-invariant records into JIT codegen (currently analysis-only).
- Bulk `get/set_array_region` host API to kill per-byte virtual calls on zip/Unsafe hot paths.
- Carrier-thread pool for virtual threads.

**Capability:**
- Finish ECDSA/DSA JAR-signature verification (currently fail-closed unsupported).
- Wire the AWT platform backends so `setVisible` actually opens a window, or document the headless-only scope.
- Decide the GPU story: either invest in `jit-cuda`/`cuda-bridge`/`craton-gpu` (real per-stream concurrency,
  occupancy-based block sizing, broader lowering) or descope the `gpu-lowering` dead feature.
- Real `sysinfo`-backed JFR environment events; complete JEP-290 deserialization limits.

**Project hygiene:**
- Make `cargo test` green (the 4 failing crates) and gate CI on it + `cargo clippy -D warnings`.
- Cut an actual release from the growing Unreleased block; automate license/SBOM checks.
