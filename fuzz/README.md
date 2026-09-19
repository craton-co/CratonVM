<!--
SPDX-License-Identifier: Apache-2.0
Copyright 2024-2026 Craton Software Company
-->

# CratonVM Fuzz Targets

This crate hosts the libFuzzer-driven coverage-guided fuzz targets for
CratonVM's untrusted-byte parsers. It is **not** a workspace member and
does not inherit workspace lints. The `fuzz_target!` macro expands into
code such as `#![no_main]` and libFuzzer entry-point symbols, which would
otherwise trip the production lint set.

## Targets

These names are the Cargo `[[bin]]` names used by `cargo fuzz`.

| Target                   | Surface (entry point)                                                          | What gets fuzzed                                                                                                                |
| ------------------------ | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| `fuzz_classfile`         | `cratonvm_reader::read_class` + `force_decode_all`                             | Magic, version, constant pool, fields, methods, interfaces, and every lazy-attribute body decoder.                                |
| `fuzz_read_class`        | `cratonvm_reader::read_class_arc`                                              | Arc-backed class parsing, matching the VM classloader's zero-copy path.                                                          |
| `fuzz_constant_pool`     | `read_class` → `read_constant_pool`, then every `ConstantPool` accessor        | Constant pool in isolation: bogus counts, index 0, index > count, cross-referencing cycles, category-2 slots, modified UTF-8.    |
| `fuzz_attribute_nesting` | `cratonvm_reader::decode_attribute` + `attribute::validate_attribute_shape`    | `Code`-in-`Code`, `Record`-in-`Record`, nested annotation `element_value`s, and `TypeAnnotation` `type_path`s.                    |
| `fuzz_stack_map`         | `cratonvm_reader::stack_map::StackMapTable::parse`                             | Stack-map verification frames and verification-type entries.                                                                    |
| `fuzz_instruction`       | `cratonvm_reader::instruction::Instruction::decode`                            | Bytecode opcodes, operands, `wide`, `tableswitch`, and `lookupswitch` decode paths.                                              |
| `fuzz_descriptor`        | `MethodDescriptor`, `FieldType`, and generic-signature parsers                 | JVM descriptors and recursive generic signatures from constant-pool UTF-8 data (raw bytes).                                      |
| `fuzz_jni_descriptor`    | the same parsers plus `signature::*_cached`                                    | Structured JNI/descriptor shapes: `[[[[…` past 255 dims, unterminated `L…;`, empty names, 300-argument lists, invalid chars.      |
| `fuzz_verifier`          | `cratonvm_classloading::ClassManager::define_class`                            | Structural class validation, linking, and verifier entry on bounded classfile input.                                             |
| `fuzz_zip_entry`         | `cratonvm_classloading::ClassPath::{add_path, list_class_names, find_*}`       | JAR/ZIP central directory + local headers, truncation, absurd entry counts, zip64, ratio bombs, and path traversal in names.     |
| `fuzz_signed_jar`        | `cratonvm_classloading::jar_signer::*` + `ManifestInfo::parse`                 | `MANIFEST.MF`, `.SF`, and PKCS#7 signer blocks: unsupported algorithms, malformed digests, missing entries, huge attributes.     |
| `fuzz_jimage`            | `cratonvm_reader::JImageReader::from_bytes` + `iter_entries` + `find_resource` | jimage container header, redirect, offsets, locations, strings sections, and perfect-hash lookup path.                           |
| `fuzz_jfr_chunk`         | `cratonvm_jfr::{read_jfr_header, read_events}` + `dump` varint codec           | JFR chunk headers, offset-driven region walk, per-record size accounting, and compressed-int/long edge cases.                    |
| `fuzz_asn1`              | `cratonvm_native_builtins::jca::asn1::*` and `x509_manager::parse_certificate` | TLV header walker, OID decoder, AlgorithmIdentifier, SubjectPublicKeyInfo, Extensions, and X.509 certs.                          |
| `fuzz_keystore`          | `cratonvm_native_builtins::keystore::{load_jks, load_pkcs12, load_keystore}`   | JKS HMAC-SHA1 keystore format, PKCS#12, and the magic-sniffing dispatcher.                                                      |
| `fuzz_tls_record`        | `cratonvm_native_builtins::tls::tls_impl::TlsRecordLayer::decode_record`       | 5-byte TLS record header parse, fragment-length clamp, and encode/decode round-trip.                                            |
| `difftest_bytecode`      | `cratonvm_difftest::mutate::{numeric_constants, mutate_constant}`              | Constant-pool walk and length-preserving in-place numeric-constant mutation for the differential fuzz tier.                      |

### Oracles

Every target enforces **panic-freedom**: arbitrary input may produce
`Err(_)`, but must never panic, abort, or unwind through the parser.

Several targets enforce more than that, because a parser that returns the
*wrong answer* without crashing is the more dangerous failure mode:

| Target                   | Additional oracle                                                                                                                                                                                                    |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fuzz_constant_pool`     | Pool size never exceeds the `u16` count **or** the number of wire bytes supplied (one tag byte minimum per slot). Category-2 constants are followed by a tombstone and never occupy the last slot (JVMS §4.4.5).       |
| `fuzz_attribute_nesting` | A nest far past the depth cap must be rejected — **and** a shallow nest built by the same generator must decode, so the rejection cannot pass vacuously.                                                              |
| `fuzz_jni_descriptor`    | `Display` → `parse` round trip on every parsed `FieldType` / `MethodDescriptor`. 255 array dimensions parse; 256 do not. Cached and uncached signature parsers agree (same shape, cache flushed first).               |
| `fuzz_zip_entry`         | Traversal-shaped names that the archive *really contains* must not resolve through `find_class` / `find_resource` / `contains_resource`. Inflated entries respect the 512 MiB clamp and DEFLATE's 1032:1 ratio bound.  |
| `fuzz_signed_jar`        | `verify_signer_block` against a zero-anchor `TrustStore` must return `None`. `verify_sf_binds_manifest` never accepts a `.SF` with no `*-Digest-Manifest`. `digest_matches` rejects an empty or wrong-width digest.    |
| `fuzz_jfr_chunk`         | Varint encode → decode is the identity and reports exactly the bytes written; a decode never claims more than 10 bytes nor depends on trailing bytes. `read_events` never materialises an unregistered event type.    |
| `difftest_bytecode`      | The mutator is length-preserving.                                                                                                                                                                                    |

`difftest_bytecode` is the fast, panic-only half of the semantic
differential fuzzer described in
`docs/feature-designs/differential-fuzzer.md`; divergent inputs are promoted
out-of-process to `difftest mutate`, which forks `java` and diffs CratonVM
against HotSpot. libFuzzer's in-process model forbids forking in the hot loop.

### Deliberate gaps

* **JNI symbol-name mangling.** `jni_short_name` / `jni_long_name`
  (`vm/src/vm/vm_object.rs:1671`, `vm/src/native/jni.rs:4737`) are not
  fuzzed. They are pure string functions, but reaching them means adding
  `cratonvm-vm` — and with it the JIT, GC, and threading crates — to this
  standalone fuzz workspace. `fuzz_jni_descriptor` covers the *signature*
  half of the JNI surface, which is where the parsing lives.
* **ZIP container parsing itself** is the `zip` crate's, not ours.
  `fuzz_zip_entry` targets everything CratonVM layers on top: nested/fat-JAR
  recursion, the decompression clamp, and the entry-name filters.

## Acceptance criterion

A target is considered to have passed when it has accumulated **24 hours of
aggregate fuzzing with zero crashes, timeouts, OOMs, or differential
misverifications**. Aggregate means summed across runs and across machines
for a given commit — twenty-four 1-hour shards count. The `-timeout` and
`-rss_limit_mb` flags below are what turn "hang" and "OOM" into reportable
libFuzzer findings rather than into a job that silently wedges, so a run
without them does not count toward the budget.

Any artifact written to `fuzz/artifacts/<target>/` resets that target's
clock: fix the bug, commit the minimized reproducer into
`fuzz/corpus/<target>/`, and restart the count.

## Running

Requires the nightly toolchain because libFuzzer support is nightly-only:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

# What is available:
cargo +nightly fuzz list

# The standard per-target shard (matches the acceptance criterion's flags):
cargo +nightly fuzz run <target> -- -max_total_time=3600 -timeout=10 -rss_limit_mb=4096

# Concretely:
cargo +nightly fuzz run fuzz_zip_entry -- -max_total_time=3600 -timeout=10 -rss_limit_mb=4096

# Run indefinitely (Ctrl-C to stop):
cargo +nightly fuzz run fuzz_classfile

# Bound by iterations instead of time:
cargo +nightly fuzz run fuzz_jimage -- -runs=1000000

# A different sanitizer (default is AddressSanitizer):
cargo +nightly fuzz run fuzz_tls_record --sanitizer=thread
```

### Every target in one pass

`run-all.sh` (POSIX) and `run-all.ps1` (PowerShell) enumerate targets via
`cargo fuzz list` — so a newly added `[[bin]]` is picked up with no second
list to maintain — run each for a configurable duration, and summarise which
ones produced artifacts. Both exit non-zero if any target crashed.

```sh
./run-all.sh              # 3600s per target (the acceptance tier)
./run-all.sh 300          # 300s per target (smoke)
./run-all.sh 300 fuzz_zip_entry fuzz_signed_jar   # a subset

FUZZ_TIMEOUT=30 FUZZ_RSS_MB=8192 FUZZ_JOBS=4 ./run-all.sh 3600
```

```powershell
.\run-all.ps1 -Duration 300
.\run-all.ps1 -Duration 3600 -Targets fuzz_zip_entry,fuzz_signed_jar -Jobs 4
```

Crash artifacts land in `fuzz/artifacts/<target>/` (cargo-fuzz's default
`-artifact_prefix`); per-target logs go to `fuzz/artifacts/logs/<target>.log`.

Linux is the canonical fuzzing host. `run-all.ps1` exists so a Windows
checkout can drive smoke runs, but it needs an `x86_64-pc-windows-msvc`
nightly with the sanitizer runtime present.

### Replaying the committed corpus (the CI tier)

`replay-corpus.sh` is the bounded counterpart to `run-all.sh`: it executes
every committed seed and every committed crash reproducer exactly once
(`-runs=0`) and exits. It finishes in seconds and finds nothing new — that
is not its job. Its job is to keep the corpus load-bearing:

```sh
./replay-corpus.sh                     # every target
./replay-corpus.sh fuzz_zip_entry      # one target
```

It exits non-zero when

* a committed input crashes, hangs past `-timeout`, or trips one of the
  property assertions — so a fixed bug stays fixed on every commit, and a
  seed that no longer matches its target's per-input layout is a failure
  rather than a file nobody notices; **or**
* a target has **no committed seeds at all**. That is the state that makes a
  coverage-guided campaign near-useless, and it is much easier to prevent
  than to notice. `ALLOW_EMPTY_CORPUS="<target> …"` exempts a target; every
  exemption needs a matching entry in
  `docs/known-issues/c2/fuzzing-state.md`.

## Corpora

Each target keeps its corpus under `fuzz/corpus/<target>/` and its crash
artifacts under `fuzz/artifacts/<target>/`. Every target has committed
hand-built seeds, regenerated by
[`corpus/gen_seeds.py`](corpus/gen_seeds.py) — see
[`corpus/README.md`](corpus/README.md) for what each seed encodes.

`fuzz/.gitignore` keeps `seed-*` and `regression-*` tracked and ignores
everything else a campaign writes; without that, one hour of `run-all.sh`
buries a real finding under thousands of untracked SHA1-named files.

For the targets whose natural seed is a real-world file, drop samples in
directly *in addition* to the synthetic seeds: real `javac` output under
`fuzz/corpus/fuzz_classfile/`, the bootstrap `modules` jimage truncated to
a few MiB for `fuzz_jimage`, or a real self-signed PKCS#12 for
`fuzz_keystore`. The synthetic seeds are structurally valid but narrow —
they encode the shapes a hand-written generator thought of.

## Crash Triage

When libFuzzer finds a panicking input it writes the bytes to
`fuzz/artifacts/<target>/crash-<sha1>` and stops. To reproduce:

```sh
# Replay just the failing input:
cargo +nightly fuzz run fuzz_classfile fuzz/artifacts/fuzz_classfile/crash-<sha1>

# Minimize the test case (libFuzzer does a shrink pass):
cargo +nightly fuzz tmin fuzz_classfile fuzz/artifacts/fuzz_classfile/crash-<sha1>

# Inspect what coverage that input hits:
cargo +nightly fuzz cmin fuzz_classfile
cargo +nightly fuzz coverage fuzz_classfile
```

After fixing the bug, commit the minimized crash bytes into
`fuzz/corpus/<target>/regression-<short-description>` so the regression
stays in the seed corpus for local runs and future fuzz-build CI.

Note that several targets assert *properties*, not just panic-freedom (see
the oracle table above). A failure there surfaces as a failed `assert!`
rather than a segfault — read the assertion message before assuming the
input is malformed in an uninteresting way.

## OSS-Fuzz Onboarding

CratonVM is a long-running parser-heavy project, which is a natural fit for
[OSS-Fuzz](https://github.com/google/oss-fuzz) continuous-fuzzing
infrastructure. The targets in this crate are already shaped the way
OSS-Fuzz expects: one `[[bin]]` per target, `fuzz_target!` macro, and a
panic-or-assertion oracle. Onboarding is mostly project metadata.

Sketch of the files OSS-Fuzz wants under `projects/cratonvm/` in its
repository:

`project.yaml`:

```yaml
homepage: "https://github.com/craton-co/cratonvm"
language: rust
primary_contact: "security@craton.com.ar"
auto_ccs:
  - "security@craton.com.ar"
sanitizers:
  - address
  - memory
fuzzing_engines:
  - libfuzzer
main_repo: "https://github.com/craton-co/cratonvm.git"
```

`Dockerfile`:

```dockerfile
FROM gcr.io/oss-fuzz-base/base-builder-rust
RUN git clone --depth=1 https://github.com/craton-co/cratonvm.git $SRC/cratonvm
WORKDIR $SRC/cratonvm
COPY build.sh $SRC/
```

`build.sh` — note it derives the target list from `cargo fuzz list` for the
same reason `run-all.sh` does:

```sh
#!/bin/bash -eu
cd $SRC/cratonvm/fuzz
cargo +nightly fuzz build -O
for t in $(cargo +nightly fuzz list); do
    cp target/x86_64-unknown-linux-gnu/release/$t $OUT/$t
    if [ -d "corpus/$t" ]; then
        zip -jq $OUT/${t}_seed_corpus.zip corpus/$t/*
    fi
done
```

See the OSS-Fuzz
[new-project guide](https://google.github.io/oss-fuzz/getting-started/new-project-guide/)
and the
[Rust-specific instructions](https://google.github.io/oss-fuzz/getting-started/new-project-guide/rust-lang/)
for the full onboarding flow.
