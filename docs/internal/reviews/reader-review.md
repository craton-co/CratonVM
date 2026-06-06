# reader review

Scope: `C:\Projects\CratonVM\reader` — 18 `.rs` source files (~7.6 k LOC), 5
integration-test files (~1 k LOC), `Cargo.toml`, `README.md`. Read-only audit;
no source modified.

## Summary

- **HIGH** — `parse_field_signature` / `parse_class_signature` / `parse_method_signature`
  silently return `Some(garbage)` for adversarial deeply-nested generic input. The
  in-crate test `signature::tests::deeply_nested_signature_is_rejected_not_overflow`
  **fails on `cargo test`** (see `reader/src/signature.rs:615`).
- **HIGH** — 7 tests in `reader/tests/vulnerability_fixes.rs` fail: they assert
  `read_class()` rejects malformed `EnclosingMethod`/`NestHost`/`ConstantValue`/
  `ModuleMainClass`/`Code` (zero-length, oversize, exceeding-buffer) attributes,
  but the lazy-attribute pipeline never decodes class/method-level attribute
  bodies during `read_class`, so the validations that *would* surface those
  shapes never run. The reader accepts these malformed class files.
- **HIGH** — `ByteView::new(...)` at three call sites in `attribute.rs` panics
  on out-of-bounds — a Round-11 off-by-one bug already shipped once and is only
  pinned by a regression test, with no compile-time guard against re-introduction.
- **MED** — `reader/tests/wp1_7_attrs.rs` (all 6 annotation acceptance tests)
  silently no-op because the fixture path `apps/annotation_probe/*.class` does
  not exist in this checkout; the tests return early instead of failing loudly.
- **MED** — Several spec-derived `pub` types ship no doc comments (every field
  in `ClassFile`, `ClassFileMethod`, `ClassFileField`, `ConstantPoolEntry`
  variants), and `lib.rs` carries `#![allow(missing_docs)]` indefinitely.

## 1. Code review

### Bugs

- **HIGH — Generic-signature parser swallows mid-parse failures.**
  `reader/src/signature.rs:263-288` (`parse_class_type_sig`) calls
  `parse_type_args`, which calls `parse_type_arg`, which calls
  `parse_type_sig`. When the recursion-depth guard at
  `reader/src/signature.rs:232-238` fires deep inside `parse_type_args`,
  the inner `?` propagates `None` up to `parse_type_arg` which `break`s out
  of the args loop *without surfacing the failure*. The outer
  `parse_class_type_sig` then happily consumes the trailing `;` and
  returns `Some(TypeSig::Class { … })`. The in-crate regression test
  `deeply_nested_signature_is_rejected_not_overflow` at
  `reader/src/signature.rs:599-616` **fails on `cargo test -p cratonvm-reader`**:
  the test asserts the deeply-nested generic input returns `None`, but
  it returns `Some(...)`. Confirmed via `cargo test`:
  `assertion failed: parse_field_signature(&nested).is_none()` at line
  615. Downstream consumers that call `parse_class_signature_cached` for
  reflection metadata will silently observe truncated/wrong generic ASTs
  for crafted `Signature` attributes.

- **HIGH — `vulnerability_fixes` malformed-attribute tests are dead.**
  `reader/tests/vulnerability_fixes.rs:183-227` and `283-334` assert that
  `read_class()` rejects malformed class-level attributes
  (`EnclosingMethod` 2-byte, `NestHost` 4-byte, `ModuleMainClass` 3-byte,
  `ConstantValue` 1-byte) and malformed `Code` payloads (`code_length=0`,
  `code_length=65536`, `code_length` exceeding buffer). The post-Round-11
  reader keeps all class-level/method-level attributes lazy
  (`reader/src/class_reader.rs:462-523`); `read_attributes` only checks
  `attribute_length > buf.remaining()` and constructs `LazyAttribute::Raw`
  without decoding. The shape validation only runs from
  `decode_attribute_with_source_arc` at decode time
  (`reader/src/attribute.rs:702-730`), which is never invoked from
  `read_class`. **Confirmed via `cargo test`**: 7 of 13 tests in
  `vulnerability_fixes.rs` fail; the reader returns `Ok(...)` for every
  crafted input the test asserts should error.

- **HIGH — `Code.code_length` validation never fires during `read_class`.**
  The check `code_length == 0 || code_length > MAX_CODE_LENGTH` at
  `reader/src/attribute.rs:1361-1370` lives inside `decode_code_body`,
  which runs at lazy-decode time. A class file with `code_length = 0` or
  `code_length = u32::MAX` parses cleanly via `read_class` and only
  surfaces an error when a downstream consumer calls
  `LazyAttribute::decode`. The `code_length > buf.remaining()` check is
  similarly delayed. Same root cause as the bug above.

- **HIGH — `ByteView::new` panics on out-of-bounds ranges, used on hot
  path.** `reader/src/byte_view.rs:50-64` asserts `range.end <= source.len()`
  with `assert!`. Three call sites pass ranges derived from
  `body_offset + buf.position() + length`:
  `reader/src/attribute.rs:956` (StackMapTable),
  `reader/src/attribute.rs:1243` (Unknown),
  `reader/src/attribute.rs:1381` (Code bytecode).
  The Round-11 regression test
  `reader/src/attribute.rs:2892-2953` documents that a previous off-by-one
  in `decode_attributes_vec` made this assertion *fire* on every class
  with a nested `StackMapTable` inside a `Code` body — pinning the entire
  JVM. The current code is correct but the API design is brittle: any
  future change to the offset bookkeeping silently risks re-introducing
  the panic on untrusted input. Use `ByteView::try_new` and return
  `InvalidClassData` instead.

- **MED — Switch decoder accepts misaligned `next % 4` after raw `+= 1`.**
  `reader/src/instruction.rs:480-482, 519-522`: `while next % 4 != 0 {
  next += 1; }` increments unconditionally; if `next` is already past
  `code.len()`, the loop still runs (no bound check), then `read_i32`
  errors via EOF. Functionally safe but a hostile caller can drive
  `next` up by 3 bytes past EOF in a debug build before the EOF fires —
  no panic, but the padding semantics are technically wrong (JVMS says
  pad to next 4-aligned address *within the bytecode array*). Add a
  bound check or saturate to `code.len()`.

- **MED — `Newarray atype` not validated.** `reader/src/instruction.rs:598`:
  `0xbc => Instruction::Newarray(Self::read_u8(...)?)`. JVMS specifies
  atype must be 4..=11 (T_BOOLEAN..T_LONG). The reader accepts arbitrary
  u8. Out-of-range values reach the verifier/interpreter as semantic
  errors, but a parse-time reject is closer to defence-in-depth and
  aligns with how `reserved` bytes in `invokeinterface`/`invokedynamic`
  are validated at `instruction.rs:567-595`.

- **LOW — `class_reader.rs:229`: `i + 1 >= count` reads naturally but
  relies on `i < count <= u16::MAX`.** The arithmetic is fine in practice
  because the outer loop guarantees `i <= count - 1`, but a future
  refactor that decouples `i` from the loop bound could trip a debug
  overflow. Use `i.checked_add(1).map_or(true, |n| n >= count)`.

- **LOW — `ConstantPool::validate` cast to `u16`.**
  `reader/src/constant_pool.rs:189`: `let len = self.entries.len() as u16;`
  silently truncates if `entries.len() > u16::MAX`. The reader enforces
  the u16 cap at parse time, but a programmatically-built pool (tests,
  AOT cache) could exceed it and `validate` would mis-report indices as
  out of bounds. Cap defensively or use `usize` throughout.

- **LOW — `parse_field_signature` accepts empty class names** (`L;` and
  `Lpkg/;`). `reader/src/signature.rs:155-179`. Out of strict spec; the
  verifier rejects it later. Not exploitable but inconsistent with the
  rest of the parser's permissiveness story.

### Vulnerabilities

- **HIGH — Lazy decode bypasses shape validation.** See HIGH bug above.
  A crafted class file can encode a 64 KB `Code` attribute body declaring
  `code_length = 65535` and `exception_table_length = u16::MAX`. The
  reader does not catch this at parse time; downstream consumers that
  rely on `read_class` to surface malformed input will receive an
  `Ok(ClassFile)` they then walk lazily.

- **MED — `FieldType::parse` recursion-bounded but no cap on `Object`
  class name length.** `reader/src/field_type.rs:71-82`. A descriptor of
  the form `L` + (1 GB of letters) + `;` allocates a `String` of that
  size via `class_name.to_string()`. The class file's u16 Utf8 length
  bound (65535) caps it in practice, but `FieldType::parse` is `pub` and
  callable on caller-supplied strings of arbitrary length.

- **MED — `jimage.rs:758-779` `iter_entries` allocates an unbounded
  `Vec`.** A jimage with `table_length = u32::MAX` would trigger up to
  `u32::MAX` decoded entries. The constructor checks
  `redirect_offset + table_bytes <= file_len` (line 511-527), so
  `table_length` is bounded by `file_len / 4` ≈ ~16 GB for a 64 GB
  jimage on a 64-bit host. Realistic JDK images cap out at ~100k
  entries; an attacker-controlled jimage on a low-memory host could
  exhaust RAM. Cap the iteration result or cap `table_length` at parse
  time.

- **LOW — `jimage::find_resource` copies the resource into a fresh
  `Vec<u8>`.** `reader/src/jimage.rs:722`:
  `resources[start as usize..end as usize].to_vec()`. For multi-MB
  resources this is one malloc + memcpy per lookup. Return a borrowed
  slice (or `Arc<[u8]>` over the backing buffer) to match the
  zero-copy story the rest of the crate tells.

### Stubs / commented-out code

- One TODO at `reader/src/lib.rs:4` — re-enable `missing_docs` once the
  API stabilises. Acknowledged in the review's documentation section.
- Dead binding `let _base_pc = base_pc;` at
  `reader/src/instruction.rs:510` retained "for future offset
  validation". LOW.
- `MAX_EXCEPTION_TABLE_COUNT` comment at `reader/src/class_reader.rs:37-39`
  notes the constant was removed; the comment is stale. LOW.

No `todo!()`, `unimplemented!()`, or `panic!("not implemented")` in
production code.

### Performance

- **LOW — `decode_attribute_body` dispatch ladder** `attribute.rs:791-855`
  contains 30 sequential `Arc::ptr_eq` comparisons. For an attribute name
  that doesn't match any canonical arc (e.g. vendor attributes) we walk
  all 30 before falling through to `&**name`. A precomputed
  `HashMap<*const str, &'static str>` keyed by pointer would be O(1).
  The current implementation is already a hot-path win over `&str`
  comparison; the ladder is a micro-optimisation opportunity.
- **LOW — `signature::SigParser::read_ident`** at
  `reader/src/signature.rs:155-166` uses `String::from_utf8_lossy`
  followed by `.into_owned()`, allocating a fresh `String` per
  identifier. The input bytes are already known to be ASCII (we only
  match `[a-zA-Z0-9_$/.]`), so `unsafe { std::str::from_utf8_unchecked
  }` + `.to_string()` (or, better, `Arc<str>` interning) avoids the
  conversion cost.
- **LOW — `attribute.rs:1057-1066` `decode_target_info`** allocates
  small `Vec<u8>` (1-3 bytes) per type annotation. Could use
  `SmallVec<[u8; 8]>` to avoid heap churn on annotation-heavy classes.
- **LOW — `jimage::iter_entries` builds a `HashSet<usize>`** with
  default hasher; `FxHashSet` would be faster (the file already pulls in
  `rustc-hash` via signature cache, so no new dep).

## 2. Tests

### Inventory

| Location | Tests | Status |
|---|---|---|
| `src/buffer.rs` | 6 | all pass |
| `src/byte_view.rs` | 7 | all pass |
| `src/class_access_flags.rs` | 0 | — |
| `src/class_file.rs` | 12 | all pass |
| `src/class_file_version.rs` | 3 | all pass |
| `src/class_reader.rs` | 12 (incl. T10 intern tests) | all pass |
| `src/class_reader_error.rs` | 0 | — |
| `src/constant_pool.rs` | 9 | all pass |
| `src/field.rs` | 8 | all pass |
| `src/field_type.rs` | 6 | all pass |
| `src/instruction.rs` | 20 | all pass |
| `src/jimage.rs` | 17 | all pass |
| `src/method.rs` | 10 | all pass |
| `src/method_descriptor.rs` | 6 | all pass |
| `src/signature.rs` | 4 | **1 FAIL** (`deeply_nested_signature_is_rejected_not_overflow`) |
| `src/stack_map.rs` | 22 | all pass |
| `src/attribute.rs` | 60+ | all pass |
| `tests/vulnerability_fixes.rs` | 13 | **7 FAIL** |
| `tests/wp1_7_attrs.rs` | 6 | all "pass" via early-return; **fixtures missing** |
| `tests/wp_large_file.rs` | 3 | all pass |
| `tests/wp_switch_padding.rs` | 8 | all pass |
| `tests/wp_validate_wire_up.rs` | 3 | all pass |

Approximate coverage estimate: high (≈ 85–90 %) on the small modules
(`buffer`, `byte_view`, `class_file_version`, `field_type`,
`method_descriptor`, `class_access_flags`, `class_file`, `field`,
`method`, `instruction`, `stack_map`); moderate on
`attribute.rs`/`class_reader.rs` (large enums; many variants tested but
many error paths untested); moderate-high on `jimage.rs`. Subtract for
the failing tests and the silent skips and effective coverage on
high-risk paths drops materially.

### Gaps & concrete additions

- **HIGH — Wire up `force_decode_all` validation, then re-enable
  vulnerability tests.** Either `read_class_arc` should call
  `force_decode_all` (eager validation, what the tests assume), or the
  tests need to be re-written to call `force_decode_all` themselves and
  assert *that* errors. Currently they assert the wrong API.
- **HIGH — Add a fuzz target that calls
  `force_decode_all(&mut cf.attributes, &cf.constant_pool)` after
  `read_class`.** The lazy-decode escape hatch means most of the
  validation in `attribute.rs:702-730` is never exercised on randomly
  generated input. Workspace has `fuzz/` — add a target.
- **MED — Restore or stub fixture for `wp1_7_attrs.rs`.** The 6 tests
  silently no-op because `apps/annotation_probe/AnnotationProbe.class`
  is absent. Either commit the fixture, or build it via a `build.rs`
  using a synthesized class-file (the patterns in
  `vulnerability_fixes.rs` are reusable), or fail loudly with
  `unimplemented!()` on a missing fixture so the test infrastructure
  reports it.
- **MED — Add tests for `signature.rs` cache eviction semantics.**
  `SIGNATURE_CACHE_CAP = 8192` and the FIFO eviction logic at
  `reader/src/signature.rs:460-468` have no test that demonstrates the
  cap actually evicts (build 8193 distinct signatures, parse all,
  verify map.len() == 8192).
- **MED — Add tests for `jimage::find_resource` on a compressed
  resource path.** `JImageError::Compressed` has no test coverage; the
  builder helper at `reader/src/jimage.rs:1056-1066` doesn't currently
  emit `compressed_size > 0`. A short test that hand-builds a location
  with `compressed_size > 0` and asserts `find_resource` returns
  `Err(JImageError::Compressed(_))` would pin the contract.
- **MED — Add an unsupported-tag test for the constant pool.**
  `reader/src/class_reader.rs:345-347` errors on `InvalidConstantPoolTag`
  but no test covers the rejection (e.g. tag 2, 13, 14 are
  unallocated). Synthesize a class with tag=2 and assert
  `read_class` errors.
- **MED — Add a class-pool-truncation test.**
  `read_constant_pool` at `reader/src/class_reader.rs:175-369` has the
  defensive `entries.len() != count` check on the way out (line 358);
  no test verifies that path fires when the parser produces a different
  number of entries than declared.
- **MED — Add a `wide` opcode test for the rare opcodes** (`iload`,
  `lload`, `fload`, `dload`, `aload`, `istore`, `lstore`, `fstore`,
  `dstore`, `astore`, `ret`). The current
  `decode_wide_iload`/`decode_wide_iinc` tests at
  `reader/src/instruction.rs:904-925` only cover 2 of the 11.
- **MED — Property-test the bytecode decoder.** A `proptest` target
  that feeds random `Vec<u8>` to `Instruction::decode` and asserts
  "no panic" would catch any future overflow/alignment bug. The
  workspace already pulls `proptest` as a `dev-dependency` in
  `types/Cargo.toml`; mirror it here.
- **LOW — `class_file_version::is_supported` boundary cases.** Tests
  cover major=44, 45, 70 but not the inclusive boundary at 69. Add an
  explicit `assert!(ClassFileVersion::JAVA_25.is_supported())` and
  `assert!(!ClassFileVersion::new(70, 0).is_supported())` already
  exists.

### Quality

- The unit tests in `src/*` are well-structured and idiomatic; each
  test focuses on one variant/error path. The use of `MockNativeContext`
  pattern from sibling crates would let attribute-decode tests reuse
  fixture builders.
- Integration tests pin specific bugs (`vulnerability_fixes.rs`,
  `wp_switch_padding.rs`, `wp_large_file.rs`) which is good — they
  function as regression docs.
- Several `unwrap()` / `expect()` calls in tests; this is acceptable
  for tests. No `unwrap()` in production code paths.

## 3. Documentation

### What exists

- Crate-level `//!` rustdoc in `lib.rs:8-21` lists module purposes.
- Per-module `//!` headers on `attribute.rs`, `byte_view.rs`,
  `class_reader.rs`, `instruction.rs`, `jimage.rs`, `signature.rs`,
  `stack_map.rs`. All explain their purpose and link to JVMS sections.
- Most `pub fn`/`pub struct`/`pub enum` carry doc comments.
- `README.md` (43 lines): scope, non-goals, usage example, status.
- Many in-source comments reference the JVMS section (e.g.
  `"JVM spec 4.7.10"`) — excellent.

### Gaps

- **`lib.rs:6` `#![allow(missing_docs)]` is a documentation deferral.**
  The TODO is honest, but the allow is global; once the crate enters
  semver-stable land it should be tightened.
- **`ConstantPoolEntry` variants** at `reader/src/constant_pool.rs:11-93`
  document each variant's tag/JVMS section but the inner field semantics
  are sparse (e.g. `MethodHandle::reference_kind` has no doc; the JVMS
  table 4.4.8 should be referenced).
- **`ClassFile` fields** at `reader/src/class_file.rs:22-33`: only the
  struct-level doc is present; each field is undocumented. `version`,
  `access_flags`, etc. are inferable from their type but
  `super_class: Option<Arc<str>>` semantics (None for java/lang/Object)
  is documented in `read_class_arc` only.
- **`Attribute` variants** at `reader/src/attribute.rs:80-215`: most
  have one-line docs; the inner field `class_index`/`method_index` etc.
  on `EnclosingMethod` are bare with no JVMS reference.
- **`StackMapFrame` variants** at `reader/src/stack_map.rs:67-105`
  document the tag range and offset_delta formula; good.
- **No CHANGELOG / RELEASE NOTES.** The crate has clearly been through
  multiple "round" refactors (R7, R8, R9, R11 are mentioned throughout
  code comments). A summary changelog for downstream consumers would
  help.
- **`Instruction` enum** at `reader/src/instruction.rs:18-217` has
  category headers (`// Constants`, `// Loads`) but variants are
  undocumented. JVMS Table 6.5 is referenced once in the `decode`
  comment but not per-variant.
- **Module-level docs on `class_access_flags.rs`, `class_reader_error.rs`,
  `class_file_version.rs`, `field.rs`, `method.rs`, `method_descriptor.rs`,
  `class_file.rs`, `constant_pool.rs`** are absent — only struct/fn
  docs.

### Consistency with workspace

- The crate's `README.md` references
  `https://github.com/craton-co/cratonvm` and asserts Apache-2.0 with
  copyright "Craton Software Company" — matches workspace `Cargo.toml`
  and `NOTICE`.
- The crate name `cratonvm-reader` follows the workspace convention
  (`cratonvm-types`, `cratonvm-jit`, ...).

## 4. OSS readiness

### `Cargo.toml` audit

`reader/Cargo.toml` declares:
- `name = "cratonvm-reader"` ✅
- `version.workspace = true` → 0.3.0 ✅
- `edition.workspace = true` → 2021 ✅
- `rust-version.workspace = true` → 1.77 ✅
- `license.workspace = true` → Apache-2.0 ✅
- `repository.workspace = true` → github.com/craton-co/cratonvm ✅
- `keywords` → `["jvm", "java", "classfile", "parser", "bytecode"]` ✅
- `categories` → `["parser-implementations", "encoding"]` ✅ (valid)
- `description` → present ✅
- `readme = "README.md"` ✅
- `publish` — *inherits* `publish = false` from workspace; **must flip
  to `publish.workspace = true` or omit before crates.io release**.
- `authors` — *inherits* `authors = ["Craton Software Company"]`. ✅
- `documentation` — **missing**. crates.io defaults to docs.rs which is
  fine, but explicit is preferred.
- `homepage` — *inherits* from workspace. ✅

The dependency block is clean: only well-known crates (`thiserror`,
`tracing`, `bitflags`, `cesu8`, `strum`, `parking_lot`, `rustc-hash`).
Local path dep on `cratonvm-types` — fine for workspace consumption;
for crates.io publish the path dep needs a version constraint too
(`cratonvm-types = { path = "../types", version = "0.3.0" }`).

### SPDX headers

Every `.rs` file under `reader/src/` (verified) and `reader/tests/` (verified)
carries:
```
// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
```
except `tests/wp_large_file.rs`, `tests/wp_switch_padding.rs`, and
`tests/wp_validate_wire_up.rs` which lack the headers (`grep` confirmed).
These three should have headers added before publishing.

### NOTICE attribution

Workspace root has `NOTICE` (verified) attributing the work to Craton
Software Company. No third-party attribution required at the workspace
level for this crate's direct deps (all under MIT/Apache-2.0 — no
GPL/LGPL).

### Internal-only references

- No internal hostnames, credentials, IPs, or proprietary references in
  the crate source.
- Comments reference "round-N" audits (e.g. "Round-7 fix") which is
  internal terminology; harmless but a future cleanup could rephrase
  these to "Audit fix (2025-XX)".
- The `cratonvm-types` path dep is an internal sibling crate; for
  crates.io publish it would need to be published first.

### Release blockers

1. `publish` workspace setting must flip to true (currently `false`).
2. Three test files missing SPDX headers.
3. `Cargo.toml` path dep to `cratonvm-types` needs a `version =` field.
4. The 8 failing tests (1 unit + 7 integration) must be resolved
   before any "0.x release" tag.

**Verdict:** Not ready for crates.io publish. Needs test fixes (HIGH
priority — failing tests) plus the small Cargo.toml hygiene above.

## Top 5 fix priorities

1. **Fix `signature.rs` parser failure propagation** — partial parses
   must return `None`, not `Some(garbage)`. Currently fails its own
   in-crate test. (HIGH)
2. **Resolve the `vulnerability_fixes.rs` 7 failing tests** by either
   wiring `force_decode_all` into `read_class_arc` (eager validation —
   what the test author clearly intended) or updating the tests to
   call `force_decode_all` themselves. (HIGH)
3. **Replace `ByteView::new` panic-on-OOB with `try_new` + error
   propagation** at `attribute.rs:956, 1243, 1381`. The Round-11
   regression already shipped once; the API design invites a repeat.
   (HIGH)
4. **Stage or synthesize the `apps/annotation_probe/*.class` fixtures**
   so `wp1_7_attrs.rs` actually exercises annotation parsing instead
   of silently returning. (MED)
5. **Add a `proptest` target for `Instruction::decode` and
   `decode_attribute_with_source` over random bytes**, asserting no
   panics. The workspace already uses `proptest`; this would catch
   future regressions of the type the Round-11 bug represents. (MED)
