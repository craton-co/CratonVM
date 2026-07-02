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

| Target              | Surface (entry point)                                                            | What gets fuzzed                                                                                            |
| ------------------- | -------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| `fuzz_classfile`    | `cratonvm_reader::read_class` + `force_decode_all`                               | Magic, version, constant pool, fields, methods, interfaces, and every lazy-attribute body decoder.          |
| `fuzz_read_class`   | `cratonvm_reader::read_class_arc`                                                | Arc-backed class parsing, matching the VM classloader's zero-copy path.                                     |
| `fuzz_jimage`       | `cratonvm_reader::JImageReader::from_bytes` + `iter_entries` + `find_resource`   | jimage container header, redirect, offsets, locations, strings sections, and perfect-hash lookup path.      |
| `fuzz_stack_map`    | `cratonvm_reader::stack_map::StackMapTable::parse`                               | Stack-map verification frames and verification-type entries.                                                |
| `fuzz_instruction`  | `cratonvm_reader::instruction::Instruction::decode`                              | Bytecode opcodes, operands, `wide`, `tableswitch`, and `lookupswitch` decode paths.                         |
| `fuzz_descriptor`   | `MethodDescriptor`, `FieldType`, and generic-signature parsers                   | JVM descriptors and recursive generic signatures from constant-pool UTF-8 data.                             |
| `fuzz_verifier`     | `cratonvm_classloading::ClassManager::define_class`                              | Structural class validation, linking, and verifier entry on bounded classfile input.                        |
| `fuzz_asn1`         | `cratonvm_native_builtins::jca::asn1::*` and `x509_manager::parse_certificate`   | TLV header walker, OID decoder, AlgorithmIdentifier, SubjectPublicKeyInfo, Extensions, and X.509 certs.     |
| `fuzz_keystore`     | `cratonvm_native_builtins::keystore::{load_jks, load_pkcs12, load_keystore}`     | JKS HMAC-SHA1 keystore format, PKCS#12, and the magic-sniffing dispatcher.                                  |
| `fuzz_tls_record`   | `cratonvm_native_builtins::tls::tls_impl::TlsRecordLayer::decode_record`         | 5-byte TLS record header parse, fragment-length clamp, and encode/decode round-trip.                        |
| `difftest_bytecode` | `cratonvm_difftest::mutate::{numeric_constants, mutate_constant}`                | Constant-pool walk and length-preserving in-place numeric-constant mutation for the differential fuzz tier. |

The unifying property each target enforces is **panic-freedom**:
arbitrary input may produce `Err(_)`, but must never panic, abort,
or unwind through the parser.

`difftest_bytecode` additionally asserts the mutator is
**length-preserving**. It is the fast, panic-only half of the semantic
differential fuzzer described in
`docs/feature-designs/differential-fuzzer.md`; divergent inputs are promoted
out-of-process to `difftest mutate`, which forks `java` and diffs CratonVM
against HotSpot. libFuzzer's in-process model forbids forking in the hot loop.

## Running

Requires the nightly toolchain because libFuzzer support is nightly-only:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz

# Run a single target indefinitely (Ctrl-C to stop):
cargo +nightly fuzz run fuzz_classfile

# Run for a bounded time / iteration budget:
cargo +nightly fuzz run fuzz_classfile -- -max_total_time=300
cargo +nightly fuzz run fuzz_jimage -- -runs=1000000

# Run with a different sanitizer (default is AddressSanitizer):
cargo +nightly fuzz run fuzz_tls_record --sanitizer=thread
```

Each target maintains its own local corpus under `fuzz/corpus/<target>/`
and its own crash artifacts under `fuzz/artifacts/<target>/`. This
repository currently tracks only `fuzz/corpus/README.md`; seed corpus
directories are created by `cargo fuzz` as targets run. Seed a target by
copying real-world samples in, for example:
`fuzz/corpus/fuzz_classfile/Object.class` for `fuzz_classfile`, the
bootstrap `modules` jimage truncated to a few MiB for `fuzz_jimage`, or a
self-signed PKCS#12 for `fuzz_keystore`.

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

## OSS-Fuzz Onboarding

CratonVM is a long-running parser-heavy project, which is a natural fit for
[OSS-Fuzz](https://github.com/google/oss-fuzz) continuous-fuzzing
infrastructure. The targets in this crate are already shaped the way
OSS-Fuzz expects: one `[[bin]]` per target, `fuzz_target!` macro, and a
panic-only oracle. Onboarding is mostly project metadata.

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

`build.sh`:

```sh
#!/bin/bash -eu
cd $SRC/cratonvm/fuzz
cargo +nightly fuzz build -O
TARGETS="
fuzz_classfile
fuzz_read_class
fuzz_jimage
fuzz_stack_map
fuzz_instruction
fuzz_descriptor
fuzz_verifier
fuzz_asn1
fuzz_keystore
fuzz_tls_record
difftest_bytecode
"
for t in $TARGETS; do
    cp target/x86_64-unknown-linux-gnu/release/$t $OUT/$t
done
```

See the OSS-Fuzz
[new-project guide](https://google.github.io/oss-fuzz/getting-started/new-project-guide/)
and the
[Rust-specific instructions](https://google.github.io/oss-fuzz/getting-started/new-project-guide/rust-lang/)
for the full onboarding flow.
