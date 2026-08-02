# Fuzzing state (`fuzz/`)

> Status as of 2026-08-01, branch `feat/c2-review-remediation`.
> Scope: the P0 Security lane's fuzzing half — "fuzz all binary parsers".
> Companion to [`class-file-parser-hardening.md`](class-file-parser-hardening.md),
> which covers the reader's hardening by inspection.

The one-line answer: **the harness is in far better shape than the lane
brief assumed, and the campaign has still never been run.** Those are two
separate facts and they have been conflated. Seventeen targets exist, all
of them reach a real parser, and CI has been building all seventeen on
every commit for some time. Not one input has ever been executed by CI, and
there is no record of any human ever having run one either.

## What the lane brief said, and what is actually there

Verified by reading, with citations, before anything was written:

| Claim | Reality |
|---|---|
| "`fuzz/` contains 18 `libfuzzer` targets" | **17.** `fuzz/Cargo.toml:74-192` declares seventeen `[[bin]]` blocks and `fuzz/fuzz_targets/` holds seventeen files. The count of 18 appears in `class-file-parser-hardening.md:151` and in the brief; nothing has eighteen. |
| "they are not wired into CI" | **False.** `.github/workflows/ci.yml:926-942` defines a job `fuzz-smoke`, and its own comment says "Keeping this blocking prevents the standalone harness and the native-builtins feature set from drifting." It installs nightly, installs `cargo-fuzz`, and runs `cargo +nightly fuzz build`. All seventeen targets are compiled on every commit. **None is executed.** The same false claim is repeated in `reader/tests/mutation_harness.rs:11-13` — see "Cross-file corrections" below. |
| "with seed corpora" | **6 of 17 had one.** `fuzz_constant_pool`, `fuzz_attribute_nesting`, `fuzz_jni_descriptor`, `fuzz_jfr_chunk`, `fuzz_zip_entry`, `fuzz_signed_jar`. The other eleven had nothing. Closed by this pass — all seventeen now have committed seeds. |
| "no crash corpus, no coverage report" | **Confirmed.** No `fuzz/artifacts/` directory has ever existed. No coverage report exists anywhere in the tree. No document records a run. |
| "no record of any coverage-guided campaign ever having been run" | **Confirmed, and it is the finding that matters.** Everything else in this lane is scaffolding around a campaign that has not happened. |

The distinction is not pedantic. "Not in CI" implies the fix is to add a CI
job; a job exists and is blocking. The fix is to make that job *execute*
something, which is a different change with a different cost — see
[The CI proposal](#the-ci-proposal).

## Per-target census

Every API every target calls was resolved against the crate that defines
it; there are no dangling references and nothing here is dead the way
`fuzz_classfile` once was (`docs/internal/reviews/fuzz-review.md` records
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

From `docs/internal/reviews/fuzz-review.md`, which predates the current
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

## The CI proposal

Two jobs. The first is blocking and cheap; the second is scheduled and
expensive. Neither can pass without executing something, which is the
whole point — `fuzz-smoke` as it stands is a compile check wearing a
fuzzing job's name.

`fuzz/` is not a workspace member and the workflow file is not this lane's
to edit, so the YAML is reproduced here for the orchestrator to apply.

### Job 1 — replace `fuzz-smoke` (`.github/workflows/ci.yml:926-942`)

Blocking, every PR. Adds roughly two minutes to a job that already pays
the nightly + `cargo-fuzz` install cost.

```yaml
  # Fuzz build smoke + committed-corpus replay. Blocking on two counts:
  # the standalone harness must keep building against the workspace's
  # feature set, AND every committed seed and crash reproducer must still
  # execute cleanly. A fuzz job that only compiles is a compile job.
  fuzz-smoke:
    name: Fuzz build smoke + corpus replay
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install nightly Rust
        uses: dtolnay/rust-toolchain@nightly

      - uses: Swatinem/rust-cache@v2

      - name: Install cargo-fuzz
        uses: taiki-e/install-action@cargo-fuzz

      # A JDK boot image so `fuzz_verifier` resolves `java/lang/Object`.
      # Without it, linking bails before the verifier's type-merge lattice
      # and the target degrades to a class-file parse.
      - uses: actions/setup-java@v4
        with:
          distribution: temurin
          java-version: '25'

      - name: Build fuzz targets
        run: cargo +nightly fuzz build

      # Executes every committed seed and every committed crash
      # reproducer exactly once (`-runs=0`) and exits. Seconds, not hours.
      # Fails when a committed input crashes, hangs, or trips a property
      # assertion — so a fixed bug stays fixed — and when a target has no
      # committed seeds at all, which is the state that makes a
      # coverage-guided campaign near-useless.
      # Invoked as `bash <script>`, not `./<script>`: both shell runners in
      # `fuzz/` are tracked mode 100644 (`git ls-files -s`), because they
      # were authored on the Windows checkout, which does not carry the
      # exec bit. `./fuzz/replay-corpus.sh` would be "Permission denied".
      - name: Replay committed corpus
        env:
          CRATONVM_FUZZ_BOOTCP: ${{ env.JAVA_HOME }}/lib/modules
        run: bash fuzz/replay-corpus.sh

      # The seeds are generated, so a stale corpus is a real failure mode:
      # a layout change lands, nobody re-runs the generator, and the seeds
      # quietly stop matching. Regenerating and diffing catches it.
      - name: Seed corpus is up to date with its generator
        run: |
          python3 fuzz/corpus/gen_seeds.py
          git diff --exit-code -- fuzz/corpus/ \
            || { echo "::error::fuzz/corpus is stale — run python3 fuzz/corpus/gen_seeds.py and commit"; exit 1; }

      - name: Upload replay logs
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: fuzz-replay-logs
          path: fuzz/artifacts/logs/
          if-no-files-found: ignore
```

### Job 2 — the campaign (new file, `.github/workflows/fuzz-campaign.yml`)

Scheduled, not per-PR. One shard per target, 15 minutes each, in parallel:
roughly 4.25 aggregate CPU-hours per nightly run against the README's
24-aggregate-hour-per-target acceptance criterion, so a target clears that
bar in about 96 nightly runs — or immediately, by dispatching manually
with a larger `duration`.

```yaml
# SPDX-License-Identifier: Apache-2.0
# Copyright 2024-2026 Craton Software Company
name: Fuzz campaign

on:
  schedule:
    # 03:00 UTC daily. Not on PRs: a coverage-guided run is unbounded and
    # its result is a property of the commit, not of the diff.
    - cron: '0 3 * * *'
  workflow_dispatch:
    inputs:
      duration:
        description: Seconds per target
        required: false
        default: '900'
      targets:
        description: Space-separated target names (empty = all)
        required: false
        default: ''

permissions:
  contents: read

jobs:
  campaign:
    name: fuzz ${{ matrix.target }}
    runs-on: ubuntu-latest
    timeout-minutes: 45
    strategy:
      fail-fast: false
      matrix:
        target:
          - fuzz_classfile
          - fuzz_read_class
          - fuzz_constant_pool
          - fuzz_attribute_nesting
          - fuzz_stack_map
          - fuzz_instruction
          - fuzz_descriptor
          - fuzz_jni_descriptor
          - fuzz_verifier
          - fuzz_zip_entry
          - fuzz_signed_jar
          - fuzz_jimage
          - fuzz_jfr_chunk
          - fuzz_asn1
          - fuzz_keystore
          - fuzz_tls_record
          - difftest_bytecode
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@nightly
      - uses: Swatinem/rust-cache@v2
      - uses: taiki-e/install-action@cargo-fuzz
      - uses: actions/setup-java@v4
        with:
          distribution: temurin
          java-version: '25'

      # `run-all.sh` exits non-zero if the target crashed OR produced a new
      # artifact, so the job fails on a finding rather than reporting one in
      # a log nobody opens. `-timeout` and `-rss_limit_mb` are what turn a
      # hang and an OOM into reportable findings instead of a wedged job;
      # per fuzz/README.md, a run without them does not count toward the
      # acceptance budget.
      - name: Fuzz
        env:
          CRATONVM_FUZZ_BOOTCP: ${{ env.JAVA_HOME }}/lib/modules
          FUZZ_TIMEOUT: '25'
          FUZZ_RSS_MB: '4096'
        run: |
          bash fuzz/run-all.sh "${{ github.event.inputs.duration || '900' }}" ${{ matrix.target }}

      # Always, not just on failure: the log carries the coverage and
      # corpus-growth numbers, which are the record that a campaign ran at
      # all. That record is what this whole lane was missing.
      - name: Upload logs and any crash artifacts
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: fuzz-${{ matrix.target }}-${{ github.run_number }}
          path: |
            fuzz/artifacts/
          retention-days: 30
          if-no-files-found: warn

      # A crash artifact is a finding even if libFuzzer's own exit code
      # were to miss it. Belt and braces, because a fuzz job that cannot
      # fail is theatre.
      - name: Fail on any crash artifact
        if: always()
        run: |
          n=$(find fuzz/artifacts/${{ matrix.target }} -type f 2>/dev/null | wc -l)
          if [ "$n" -gt 0 ]; then
            echo "::error::${{ matrix.target }} produced $n crash artifact(s)"
            exit 1
          fi
```

### What makes each job fail

| Job | Fails on |
|---|---|
| `fuzz-smoke` | a target that no longer compiles; a committed seed or reproducer that crashes, hangs past 25 s, or trips a property assertion; a target with **zero** committed seeds; a `fuzz/corpus/` that no longer matches `gen_seeds.py`. |
| `fuzz-campaign` | any crash, hang, OOM, or property-assertion failure found during the run; any file appearing under `fuzz/artifacts/<target>/`. |

### Where the crash corpus lives

`fuzz/artifacts/<target>/crash-<sha1>` — cargo-fuzz's default
`-artifact_prefix`, uploaded from every campaign run and gitignored by
default (`fuzz/.gitignore`, added by this pass). A reproducer worth keeping
is committed deliberately:

```sh
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/crash-<sha1>
cp fuzz/artifacts/<target>/minimized-from-<sha1> \
   fuzz/corpus/<target>/regression-<short-description>
git add fuzz/corpus/<target>/regression-<short-description>
```

`replay-corpus.sh` then executes it on every commit, so the fix stays
fixed. Without the `.gitignore` an hour of `run-all.sh` leaves thousands of
untracked SHA1-named files in `git status`, which is how a real finding
gets lost — and, on the Windows checkout, how machine-specific bytes get
swept into history by a repo-wide auto-commit.

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

## Cross-file corrections needed (not this lane's files)

1. **`docs/known-issues/c2/class-file-parser-hardening.md:151`** says "18
   `libfuzzer` targets". There are 17.
2. **`docs/known-issues/c2/class-file-parser-hardening.md:154`** says they
   "are not wired into CI". They are — `ci.yml:926-942`, blocking. The
   accurate statement is that CI builds them and never runs them.
3. **`reader/tests/mutation_harness.rs:11-13`** repeats the same claim in
   a module docstring: "they require a nightly toolchain and `cargo fuzz`,
   so they do not run in CI". Same correction. This one matters more than
   the doc, because it is the first thing a reader of the mutation harness
   sees and it understates what already exists.
4. **`classloading/src/class_path.rs:1024,1035`** — `is_safe_resource_name`
   is `pub(crate)` and `is_safe_class_name` is private, which forced
   `fuzz_zip_entry` to hand-mirror both predicates. Making them `pub` and
   having the target call them directly removes a silent-drift hazard: as
   written, tightening the real predicate makes the fuzz assertion quietly
   weaker rather than failing.
5. **`classloading/src/jar_signer.rs`** — the multi-`SignerInfo` case wants
   an ordering-invariance `#[test]`, not a fuzz target. See above.
6. **Exec bits.** `fuzz/run-all.sh` and `fuzz/replay-corpus.sh` are tracked
   mode `100644`; the Windows checkout cannot set the bit. The YAML above
   works around it with `bash <script>`, which is the robust form anyway.
   `git update-index --chmod=+x fuzz/*.sh` from a POSIX host is the tidier
   fix if the orchestrator wants it.

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

## Campaign log

Empty by design: nothing has ever been run. Append one row per completed
shard. This table, not the CI badge, is the record.

| Date | Commit | Target | Duration | Host | Result |
|---|---|---|---|---|---|
| — | — | — | — | — | *no campaign has ever been run* |
