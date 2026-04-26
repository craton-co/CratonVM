# RustJVM — Configuration Reference

## Command-Line Options

```
rustjvm-cli [OPTIONS] <CLASS_NAME> [ARGS...]
```

| Option | Description | Default |
|--------|-------------|---------|
| `--classpath <PATH>` / `-c <PATH>` | Directories and JARs to search for `.class` files. Separator: `;` (Windows) or `:` (Unix). | `.` (current directory) |
| `--Xmx <SIZE>` | Maximum heap size. Accepts `k`, `m`, `g` suffixes. | `256m` |
| `--verbose:class` | Print class loading trace to stderr. | Off |
| `--verbose:gc` | Print GC activity to stderr. | Off |
| `--Xbootclasspath <PATH>` | Override bootstrap classpath. | Auto-detected |
| `--java-home <PATH>` | JDK installation for boot/ext classpath discovery. | `JAVA_HOME` env var |
| `--nojit` | Disable JIT compilation (interpreter only). | JIT enabled |
| `--noverify` | Skip bytecode verification. **Not recommended.** | Verify enabled |
| `--synthetic-jdk <BOOL>` | Use synthetic (Rust-implemented) stdlib. | `true` |

## Internal Configuration (`VmConfig`)

These settings are compiled into the VM and can be changed in `vm/src/config.rs`:

| Setting | Value | Description |
|---------|-------|-------------|
| `jit_threshold` | 100 | Method invocations before JIT compilation |
| `osr_threshold` | 10,000 | Loop back-edges before On-Stack Replacement |
| `gc_nursery_size` | 16 MB | Young generation semi-space size |
| `gc_old_gen_size` | 64 MB | Old generation initial size |
| `tlab_size` | 64 KB | Thread-Local Allocation Buffer size |
| `max_stack_depth` | 512 | Maximum call stack frames before StackOverflowError |
| `gc_adaptive_expansion` | true | Expand heap when GC reclaims < 25% |
| `simd_enabled` | auto | AVX2 SIMD (runtime detected via CPUID) |
| `use_synthetic_jdk` | true | Use Rust-side stdlib implementations |
| `audit_missing_natives` | false | Log missing native method calls instead of failing |

## GC Configuration

| Collector | Selection | Description |
|-----------|-----------|-------------|
| Semi-space (default) | Always active | Generational copying GC with TLABs |
| G1 | `VmConfig.use_g1` | Region-based collector (experimental) |
| ZGC | `VmConfig.use_zgc` | Low-latency concurrent collector (experimental) |

## JIT Compiler Configuration

| Setting | Value | Description |
|---------|-------|-------------|
| Optimization rounds | 26 | Number of JIT optimization passes |
| Inlining threshold | 35 bytecodes | Max callee size for inlining |
| Loop unrolling factor | 2x | Unroll factor for small loops |
| SIMD vectorization | AVX2 | Auto-detected; falls back to SSE2 |
| Register allocator | Graph-coloring | Chaitin-Briggs with coalescing |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `JAVA_HOME` | JDK installation path (used by `--java-home` default) |
| `RUST_MIN_STACK` | Minimum thread stack size (set to `8388608` for deep recursion tests) |
| `RUST_LOG` | Tracing log level (`trace`, `debug`, `info`, `warn`, `error`) |
