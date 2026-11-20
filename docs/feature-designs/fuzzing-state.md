# Fuzz harness

The standalone `fuzz/` workspace contains libFuzzer targets for class files,
JImage, descriptors, bytecode, stack maps, verification, cryptographic inputs,
JFR, ZIP, signed JARs, and differential bytecode execution. Target declarations
in `fuzz/Cargo.toml` are the authoritative inventory.

```sh
cargo +nightly fuzz build
bash fuzz/run-all.sh
bash fuzz/replay-corpus.sh
```

See [fuzz/README.md](../../fuzz/README.md) for arguments and prerequisites.
Committed seeds live under `fuzz/corpus/`. The standalone workspace mirrors
required source patches; it is not a root workspace member and is not published.

A successful build proves target compilation. It does not establish executed
coverage, campaign duration, or absence of defects. Retain corpus inputs,
reproducers, toolchain, and source identity with local run artifacts. The
[semantic differential harness](differential-fuzzer.md) separately compares
Java-visible behavior with HotSpot.
