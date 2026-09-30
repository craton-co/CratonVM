# Fuzzing harness (`fuzz/`)

**Status:** Partial — the harness is complete and CI builds all of it; **no
fuzzing campaign has ever been run.**

## What it does today

Those are two separate facts and conflating them is the standing hazard here.

- **Seventeen libfuzzer targets exist**, declared in `fuzz/Cargo.toml` with a
  matching file each in `fuzz/fuzz_targets/`: `fuzz_classfile`, `read_class`,
  `fuzz_jimage`, `fuzz_stack_map`, `fuzz_instruction`, `fuzz_descriptor`,
  `fuzz_verifier`, `fuzz_asn1`, `fuzz_keystore`, `fuzz_tls_record`,
  `fuzz_constant_pool`, `fuzz_attribute_nesting`, `fuzz_jni_descriptor`,
  `fuzz_jfr_chunk`, `fuzz_zip_entry`, `fuzz_signed_jar`, `difftest_bytecode`.
  Every one of them reaches a real parser.
- `fuzz/` is a **standalone workspace**, not a root member, and carries its own
  `[patch.crates-io]` mirror.
- Seed corpora are committed under `fuzz/corpus/<target>/`; manual runners are
  `fuzz/run-all.ps1`, `run-all.sh` and `replay-corpus.sh`.
- **CI builds, and only builds.** The `fuzz-smoke` job runs exactly
  `cargo +nightly fuzz build`. It is blocking, which is what stops the
  standalone harness rotting — but **zero inputs are executed**, and there is
  no scheduled or long-running fuzz workflow.

## What is not built yet

- **Execution.** Not one input has been run by CI, and there is no record of a
  human having run one. Everything below about corpus quality and coverage
  gaps describes a harness that has never been driven.
- **A nightly (or otherwise long-running) job**, which is what a campaign
  needs and what the repository does not have.

The behavioural differential harness is a different thing with a different
posture — it *is* executed and blocking. See
[`differential-fuzzer.md`](differential-fuzzer.md).

## Per-target census

Every API every target calls was resolved against the crate that defines
it; there are no dangling references and nothing here is dead the way
`fuzz_classfile` once was (`fuzz-review.md` records
that historical break — `ClassFile::parse` never existed).

"Vacuous" below means: does the target exercise the parser, or something
in front of it? None of the seventeen builds its input through a
validating builder, and none bails on the first byte in a way that makes
the target pointless. Three were, however, **effectively vacuous in
practice** for want of a seed — a distinct and more insidious failure,
because the target reads as coverage and produces almost none.

| Target | Reaches | Vacuous? | Corpus (before → after) | Action |
|---|---|---|---|---|
| `fuzz_classfile` | `read_class` + `force_decode_all` over class/method/field attributes + `source_file`. The eager tables *and* every lazy attribute decoder. | No | 0 → 4 | Seeded. |
| `fuzz_read_class` | `read_class_arc`. | No, but **redundant**. `read_class` is a thin wrapper over `read_class_arc` (`reader/src/lib.rs:57`, `class_reader.rs`), and this target stops at the eager parse — it never forces the lazy decoders. Its coverage is a strict subset of `fuzz_classfile`'s. The unique surface it claims (`Arc`-source bookkeeping) is reached by `fuzz_classfile` too, via the internal copy. | 0 → 2 | Seeded; **merge candidate**. Folding it into `fuzz_classfile` behind a selector byte would free a campaign shard. Left alone here rather than deleted: deleting a target is a decision for whoever owns the campaign budget. |
| `fuzz_constant_pool` | `read_class` → `read_constant_pool` with the header and tail pinned, then every `ConstantPool` accessor across the whole index space. | No. Four structural oracles beyond panic-freedom, including the anti-OOM `cp.len() <= 1 + cp_bytes.len()` byte-accounting bound. | 3 → 3 | None. Strongest resource-exhaustion oracle in the set. |
| `fuzz_attribute_nesting` | `decode_attribute` + `validate_attribute_shape`, plus the raw body re-wrapped inside a well-formed `Code` so it routes through `decode_attributes_vec`'s length accounting. | No. Asserts the depth caps **in both directions** — a shallow nest must decode, which is what stops the deep-nest rejection passing vacuously. | 3 → 3 | None. Note the both-directions self-test runs once per process (`Once`), not per input; deliberate and documented at the call site. |
| `fuzz_stack_map` | `StackMapTable::parse` + `absolute_offsets`, plus a `Debug` walk of every frame. | No | 0 → 4 | Seeded, including the `offset_delta` overflow chain the module docstring names as its reason for existing. |
| `fuzz_instruction` | `Instruction::decode` walked from pc 0 with forward-progress enforcement, plus probes at interior offsets. | No | 0 → 4 | Seeded with the alignment-padded switches and both `wide` forms — the shapes a byte mutator effectively never assembles. |
| `fuzz_descriptor` | `MethodDescriptor::parse`, `FieldType::{parse, parse_partial}`, all three `signature::parse_*`. | No, but panic-only. | 0 → 5 | Seeded. |
| `fuzz_jni_descriptor` | The same parsers, plus the `*_cached` twins, plus harness-generated adversarial shapes. | No — **the strongest oracle in the set.** `Display` → `parse` round trip on every parsed type; the 255-dimension cap asserted in both directions; and a cached-vs-uncached differential run across **all six orderings** of the three signature shapes, because the cache-aliasing bug it guards against fired only when the rejecting shape was asked first. | 3 → 3 | None. |
| `fuzz_verifier` | `ClassManager::define_class` — parse, link, verify. | No, but **shallower than its name** by default. Its own docstring is honest about this: without `CRATONVM_FUZZ_BOOTCP`, `java/lang/Object` does not resolve, linking bails, and the type-merge lattice is never entered. Nothing in the tree sets that variable. | 0 → 2 | Seeded, **and the CI job below sets `CRATONVM_FUZZ_BOOTCP=$JAVA_HOME/lib/modules`.** `BootstrapClassFinder::new` → `ClassPath::new` → `add_path`, which sniffs and routes a jimage through `load_jimage` (`classloading/src/class_path.rs:1691,4492`), so the boot image is an acceptable value. Not verified end to end from this worktree — see "Not verified". |
| `fuzz_zip_entry` | `ClassPath::{add_path, list_class_names, find_class, find_resource, contains_resource, find_all_resource_*, read_jar_manifest}` over a staged temp file. | No — and the traversal oracle is genuinely non-vacuous: it asserts against names the archive **really contains**, which `list_class_names` reports verbatim, so a regressed name filter fires immediately. | 4 → 4 | Two hazards, both recorded rather than fixed: (1) it writes and flushes a file **every iteration**, which is disk-bound and will dominate exec/s — budget accordingly; (2) `class_name_is_hostile` / `resource_name_is_hostile` are **hand-mirrored copies** of `is_safe_class_name` / `is_safe_resource_name` (`class_path.rs:1035`, `:1024`, both `pub(crate)` or private). If the real predicate is *tightened*, the mirror silently stops asserting on the newly-hostile shapes; if it is *loosened*, the mirror produces a false failure. Making `is_safe_*` `pub` and calling it directly is the fix, and it is a `classloading/` change. |
| `fuzz_signed_jar` | `verify_signer_block`, `verify_chain`, `X509Cert::parse`, `verify_sf_binds_manifest`, `parse_manifest_entry_digests`, `digest_matches`, `TrustStore::*`, `ManifestInfo::parse`. | No. Fail-closed oracle: `verify_signer_block` against a zero-anchor `TrustStore` must never return `Some`. That is the fail-open shape that matters, and it is asserted, not assumed. | 3 → 3 | None to the target. See "Coverage gaps" for the multi-`SignerInfo` case, which this target's oracle **cannot** find. |
| `fuzz_jimage` | `JImageReader::from_bytes`, `version`, `resource_count`, `iter_entries`, two `find_resource` probes. `iter_entries` returns `Result<Vec<_>, _>`, so `let _ =` genuinely materialises the walk rather than dropping a lazy iterator. | Not by construction — but **it was vacuous in practice.** With no seed, essentially every input failed the `0xCAFEDADA` magic check in `Header::parse` and returned before touching a single section offset. | 0 → 3 | Seeded with a **real two-resource jimage**, built to mirror `reader/src/jimage::test_builder` including the multiply-then-xor FNV hash and the `redirect = -(slot + 1)` single-step assignment. Independently validated: `iter_entries` reconstructs both paths and `find_resource("/java.base/java/lang/Object.class")` — the exact string the target probes — resolves to the resource bytes. |
| `fuzz_jfr_chunk` | The compressed-int/long codec (round-trip oracle over 8-byte windows plus fixed boundary values), `read_jfr_header`, `read_events` against both a populated and an empty registry. | No | 3 → 3 | None. Same per-iteration temp-file cost as `fuzz_zip_entry`; the varint phase runs first and does not touch disk, so short inputs stay cheap. |
| `fuzz_asn1` | `asn1::{read_header, read_oid, decode_algorithm_identifier, decode_subject_public_key_info, decode_extensions}` and `x509_manager::parse_certificate`, five-way by selector byte. | No | 0 → 6 | Seeded, one per branch plus a full v3 certificate whose signature is garbage, so the parser walks the entire structure before rejecting. |
| `fuzz_keystore` | `keystore::{load_jks, load_pkcs12, load_keystore}`, three-way by selector. | No | 0 → 5 | Seeded. |
| `fuzz_tls_record` | `TlsRecordLayer::decode_record` in two configurations, plus an encode/decode round trip after a successful decode. | No | 0 → 5 | Seeded. |
| `difftest_bytecode` | `mutate::numeric_constants` and `mutate::mutate_constant`. | Not by construction — but **it was vacuous in practice.** `mutate_constant` returns `None` when the pool holds no `Integer`/`Float`/`Long`/`Double`, and the target's only assertion (length preservation) is inside the `if let Some(..)`. With no seed carrying numeric constants, the assertion essentially never ran. | 0 → 2 | Seeded with `seed-numeric-constants`, a pool holding all four numeric tags. |

**Summary: 0 of 17 vacuous by construction, 3 vacuous in practice for want
of a seed, 1 redundant.** The eleven corpus-less targets were the real
finding — a coverage-guided fuzzer handed no seed starts from random noise,
and for a format with a magic number it never gets past byte 0.

## Corpus quality

The six pre-existing corpora are genuinely good: real, hand-built,
structurally valid inputs that match each target's per-input layout, not
placeholder bytes. `seed-coherent-pool` is a constant pool whose
`this_class` and `super_class` actually resolve, so `read_class` returns
`Ok` and every accessor assertion in `fuzz_constant_pool` runs.
`seed-traversal-names` is a real ZIP whose central directory really
contains `../../../../etc/passwd.class`. `seed-absurd-count` declares
65 535 pool entries backed by three bytes.

All of it is generated by `fuzz/corpus/gen_seeds.py` (standard library
only, idempotent), which is the readable source of truth for binary seeds
that are otherwise unreviewable in a diff. This pass extended that
generator rather than checking in opaque bytes.

Two of the new seeds were validated against the implementation rather than
against the spec, because getting them wrong would have produced exactly
the silent non-coverage this document exists to call out:

* `fuzz_classfile/seed-valid-class` is **218 bytes**, which is the length
  `reader/tests/mutation_harness.rs::mutation_coverage_totals_are_exact`
  pins for its own fixture. The Python builder mirrors that fixture
  structure for structure, and the byte count agreeing is the check.
* `fuzz_jimage/seed-two-resources` was re-parsed with an independent
  reimplementation of `from_bytes` / `iter_entries` / the perfect-hash
  lookup. Both resource paths round-trip and the positive `find_resource`
  resolves.

## Coverage gaps

### The lane's own list is fully covered

Class files, JAR/ZIP central directories and local headers, `jimage`, the
JAR manifest and signature blocks (PKCS#7), stack map tables, and the
constant pool — every one has a target, and after this pass every one has
seeds. There is no gap against that list.

### The gaps that remain are elsewhere

From `fuzz-review.md`, which predates the current
seventeen targets and whose other findings are closed:

* **`parse_http_request_head` (`native-builtins/src/wildfly_undertow.rs:451`)
  — the largest remaining gap, and it is not on the lane's list.** It
  parses a raw HTTP request line and headers straight off a socket. That
  is strictly more attacker-reachable than `jimage` (which requires write
  access to `$JAVA_HOME`), and it has no target. Adding one is cheap: it
  takes `&[u8]` and returns `Result<(ParsedRequest, usize), _>`, so a
  panic-only target is four lines, and the `usize` it returns is a
  free structural oracle (the consumed length must never exceed the input).
* `parse_module_xml_bytes` (`native-builtins/src/jboss_module_xml.rs:112`).
* `parse_properties_pub` (`native-builtins/src/properties_sidetable.rs:1040`)
  — returns a `Vec`, so a bounded-output oracle applies.
* `decode_with_charset` (`native-builtins/src/charset.rs`).

### A gap no panic oracle can close

The crypto lane's finding that the signed-JAR path takes the first
`SignerInfo` and ignores the rest is **real, and it is already documented**
— `classloading/src/jar_signer.rs:110-111` ("we only verify the first
`SignerInfo`; multi-signer JARs collapse to 'first signer wins'") and again
at `:353`. The implementation is `signer_infos.next()` at `:476-481`.

`fuzz_signed_jar` cannot find it, and no amount of running it will. It is
not a crash, and it is not a fail-open against a zero-anchor trust store,
which are the two things that target asserts. Finding it needs a
**structure-aware differential**: build a `SignedData` carrying two
`SignerInfo`s, one valid and one not, and assert the verdict does not
depend on their order. That is a targeted test, not a fuzz target — the
input space that reaches it is too structured for a mutator to find and
too small to need one. Recommended as a `#[test]` in
`classloading/src/jar_signer.rs`, not as target #18.

## Nightly is a hard blocker, and here is the honest fallback

libFuzzer instrumentation is nightly-only. `cargo +nightly fuzz` is not
optional and will not become optional. That means:

* **Nothing under `fuzz/` runs on stable, ever** — including the corpus
  replay above, which still needs the instrumented binaries.
* Any host without a nightly toolchain (this Windows checkout among them)
  cannot verify a change to a fuzz target beyond reading it.
* Both CI jobs above pay a nightly install on every run.

The counterweight already exists and already runs: **the deterministic
mutation harness at `reader/tests/mutation_harness.rs`.** It is an ordinary
`cargo test` — no `rand`, no seed, no wall-clock bound, identical on every
host — and it sweeps 2 819 mutants across four systematic families over one
218-byte seed, driven past `read_class` through `force_decode_all` and
`verified_code` so the lazy attribute decoders, the stack-map parser and
the instruction decoder are all in the blast radius. Its stated limit is
that it explores one seed's neighbourhood only.

Which targets are better expressed that way:

| Target | Verdict |
|---|---|
| `difftest_bytecode` | **Yes.** Its whole oracle is "the mutator is length-preserving and does not panic" over a constant-pool walk. That is a property, not a search: a deterministic sweep over a handful of pools with every numeric tag, mutated at every constant, would cover it exhaustively in milliseconds on stable. The libFuzzer target adds a mutator on top of a mutator. |
| `fuzz_attribute_nesting` | **Half.** Its depth-cap assertions already run once per process rather than per input, precisely because they do not depend on the fuzzer's bytes. That half is a `#[test]` wearing a fuzz target's clothes and would run on every commit if it were one. The raw-body phase is genuinely search-shaped and should stay. |
| `fuzz_jni_descriptor` | **Half.** `structural_limits_self_test` is likewise input-independent and `Once`-guarded. The cached-vs-uncached differential across six orderings is search-shaped and should stay. |
| `fuzz_stack_map`, `fuzz_instruction` | **Yes, as a second harness.** Both consume a flat byte array with no magic number and no checksum — exactly the shape a deterministic sweep handles well. A `reader/tests/` sweep over the seeds now committed under `fuzz/corpus/fuzz_stack_map/` and `fuzz_instruction/` would run on stable, on every commit, on Windows. |
| Everything else | **No.** ZIP, jimage, PKCS#7, JFR and X.509 all have magic numbers, length prefixes or checksums that a deterministic bit-flip sweep destroys on the first mutation. Coverage-guided search is the right tool and there is no stable substitute. |

A bounded deterministic sweep that actually runs does beat an unbounded
campaign that does not. It does not replace one that does.

## Not verified

Stated plainly, because this lane is about not mistaking scaffolding for
results:

* **Nothing here was built or run.** No `cargo build`, `cargo test`,
  `cargo fuzz` or `cargo check` was executed — this worktree is shared with
  eight concurrent agents and building was out of scope. Every API every
  target calls was resolved by reading the defining crate, and the
  generated seeds were validated with independent Python reimplementations
  of the parsers they target. That is stronger than a compile check for
  seed correctness and weaker than one for the targets themselves.
* **`CRATONVM_FUZZ_BOOTCP=$JAVA_HOME/lib/modules` is reasoned, not
  demonstrated.** `BootstrapClassFinder::new` forwards to `ClassPath::new`
  → `add_path`, which sniffs and routes a jimage through `load_jimage`
  (`class_path.rs:1691,4492`). Whether that is enough for
  `java/lang/Object` to resolve inside `define_class` was not observed.
  If it is not, `fuzz_verifier` stays shallow and the fix is a loose-class
  boot directory instead.
* **No campaign has been run by this pass either.** This pass made one
  runnable and recorded; it did not run one. The acceptance criterion in
  `fuzz/README.md` — 24 aggregate hours per target with zero findings —
  stands at **zero hours for all 17 targets**.

