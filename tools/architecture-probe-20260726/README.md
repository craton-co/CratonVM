# Architecture probes (2026-07-26)

These small probes support the corresponding architecture review. They are not
a general benchmark suite and their absolute timings should not be published as
product claims. Use the relative shapes to validate specific architecture
questions.

The Java runner measures:

- identical no-JIT bytecode in a default-package class and an
  `org.springframework.*` class, exposing CratonVM's package-selected dual
  interpreter paths;
- one, four, and sixteen receiver types at one interface call site;
- retained allocation of objects with eight primitive instance fields;
- preallocated exception throw/catch, uncontended monitor, and native-call
  boundary costs.

`check-interpreter-equivalence-20260727.sh` additionally runs an opcode-rich
kernel through the verified raw-byte path and the `--noverify` decoded
fallback and requires byte-identical output:

```bash
tools/architecture-probe-20260726/check-interpreter-equivalence-20260727.sh \
  /data/data/bin/cratonvm-complete-remediation-20260726-r3 \
  /home/victor/jdk25
```

Every case runs in a fresh process, pins one CPU, alternates HotSpot and
CratonVM within each repetition, and records the checksum. Runs are invalid if
deterministic checksums differ.

The focused interface-dispatch gate additionally requires JIT execution to
beat `--nojit` for mono-, four-way poly-, and sixteen-way megamorphic shapes,
and checks that a four-receiver unrolled call site stops entering the cache
helper after warm-up:

```bash
tools/architecture-probe-20260726/check-interface-jit-performance-20260727.sh \
  -Exe /data/data/bin/cratonvm-complete-remediation-20260726-r16 \
  --java-home /usr/lib/jvm/java-17-openjdk-amd64 \
  --iterations 1000000 --reps 3 --cpu 13
```

The focused monitor gate exercises a javac synchronized block through
method-entry JIT compilation. It checks JIT/`--nojit` checksums, forces an
exception from inside the protected region, verifies catch-all cleanup
rethrows and releases the lock, and requires both monitor methods to reach the
compiler:

```bash
tools/architecture-probe-20260726/check-monitor-jit-path-20260727.sh \
  -Exe /data/data/bin/cratonvm-architecture-final-convergence-20260727-r1 \
  --java-home /home/victor/jdk25 \
  --iterations 500000 --cpu 13
```

Build CratonVM under a unique name and run:

```bash
CARGO_TARGET_DIR=/data/data/target-architecture-audit-20260726 \
  cargo build --release -p cratonvm-cli --bin cratonvm
install -m 755 \
  /data/data/target-architecture-audit-20260726/release/cratonvm \
  /data/data/bin/cratonvm-architecture-audit-20260726

tools/architecture-probe-20260726/run-architecture-probe-20260726.sh \
  -Exe /data/data/bin/cratonvm-architecture-audit-20260726 \
  --java /home/victor/jdk25/bin/java \
  --java-home /home/victor/jdk25 \
  --cpu 13 --reps 3
```

Run the Rust layout probe as its own uniquely named binary:

```bash
CARGO_TARGET_DIR=/data/data/target-architecture-layout-probe-20260726 \
  cargo run --release -p cratonvm-types \
  --example cratonvm_architecture_layout_probe_20260726
```

The defaults are intentionally short enough for a shared probe host. Increase
individual workloads with `INTERP_ITERS`, `DISPATCH_ITERS`, `ALLOC_ITERS`,
`EXCEPTION_ITERS`, `MONITOR_ITERS`, and `NATIVE_ITERS` after checking host load.
