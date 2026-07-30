# Embedding `cratonvm-vm` in a Rust Application

Audience: Rust developers who want to host a JVM inside their own process —
for example, scripting a Rust application with Java plugins, embedding a
Java SDK in a Rust CLI, or running Java workloads alongside native code in
the same address space.

The first-party `cratonvm` CLI in [`vm-cli/src/main.rs`](../vm-cli/src/main.rs)
is the reference embedder; the same pattern works from any binary that
depends on `cratonvm-vm`.

## Minimal example

```rust
use cratonvm_vm::{Vm, VmConfig};
use cratonvm_vm::types::Value;

fn main() -> anyhow::Result<()> {
    let config = VmConfig::default()
        .with_classpath(vec!["target/classes".to_string()])
        .with_max_heap_size(512 * 1024 * 1024);

    let mut vm = Vm::new(config);

    // Optional: stage classes, set statics, etc., before main().
    vm.ensure_class_initialized(vm.load_class("com/example/Main")?)?;

    // Invoke `public static void main(String[] args)`.
    let args = Value::Object(None); // pass a real String[] when needed
    let _ = vm.invoke("com/example/Main", "main", "([Ljava/lang/String;)V", &[args])?;
    Ok(())
}
```

`Vm::new` performs the full bootstrap: builds the `SharedVm`, registers the
main thread (`ThreadId(0)`), loads `java.base` primordials, and emits the
startup JFR events. After it returns, the VM is at init level 1; for a full
`main()`-style run you can rely on `Vm::invoke` to drive `System.initPhaseN`
the same way the CLI does (see [`vm-cli/src/main.rs`](../vm-cli/src/main.rs)
for the explicit bootstrap loop).

## `VmConfig` highlights

The full surface is documented in [`vm/src/config.rs`](../vm/src/config.rs).
Builder-style setters return `Self`, so chains compose cleanly.

| Setter | Purpose |
|---|---|
| `with_max_heap_size(bytes)` | `-Xmx` equivalent. Default 256 MiB. See [`docs/gc-tuning.md`](gc-tuning.md). |
| `with_classpath(vec)` | Application classpath. Use `VmConfig::parse_classpath` to honour the platform separator. |
| `with_boot_classpath(vec)` / `with_ext_classpath(vec)` | Override JDK auto-discovery. |
| `with_java_home(path)` | Force a specific JDK install for jmod discovery; otherwise `JAVA_HOME` and PATH are consulted. |
| `gc_algorithm = GcAlgorithm::G1` | Switch from the default generational collector to G1. See [`docs/gc-tuning.md`](gc-tuning.md) for the trade-offs. |
| `use_compressed_oops`, `use_compact_headers` | Memory-footprint knobs; off by default. |
| `with_xverify_mode(XverifyMode::All)` | Force verification of boot classes too (compliance testing). |
| `with_aot_mode(AotMode::Training)` + `with_aot_cache_output(path)` | Project Leyden AOT cache writer (requires the `experimental-aot` feature; a build without it logs a warning and ignores the request). |
| `with_jdwp(port, suspend)` | Spawn a JDWP server thread at startup (requires the `experimental-debug` feature). |
| `with_container_support(false)` | Disable cgroup auto-sizing (`-XX:-UseContainerSupport`). |

The default config is what the CLI uses when no flags are passed — see
`VmConfig::default()` in [`vm/src/config.rs`](../vm/src/config.rs) for the
exact starting values.

## `SharedVm` lifetime and threading

`Vm` is a single-thread embedder convenience: it owns one main `JvmThread`
plus an `Arc<SharedVm>`. The real shared state lives on `SharedVm`
(see [`vm/src/vm/vm_init.rs`](../vm/src/vm/vm_init.rs)) and is meant to be
cloned and handed to additional Java threads.

- The `Arc<SharedVm>` may be cloned freely; `Vm::shared` exposes the
  underlying handle.
- For each *additional* Java thread, allocate a new `JvmThread`, register it
  with `shared.thread_registry`, and dispatch invocations via the free
  functions in [`vm/src/vm/vm_exec.rs`](../vm/src/vm/vm_exec.rs)
  (`invoke_shared`, `invoke_on_class_shared`). The `Vm::invoke` method is a
  thin wrapper around `invoke_shared` that pins the main thread.
- `SharedVm` itself is `Send + Sync` and is designed to outlive any one
  invocation.

## Registering custom natives

`SharedVm::native_methods` is a `NativeMethodRegistry`
(`cratonvm-native-api`). The registry is treated as *immutable after VM
construction* on hot lookup paths, so the right place to add embedder
natives is **before** `Vm::new` returns from your perspective — that is,
inside a wrapper that constructs `SharedVm`, registers extras, then builds
the `Vm` around it. See `register_io_natives`
([`native-io/src/lib.rs:3427`](../native-io/src/lib.rs)) for the
in-tree pattern.

A native is a `fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult`:

```rust
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_vm::types::Value;
use cratonvm_vm::error::MethodCallResult;

fn my_native(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = args[0].as_int().unwrap_or(0);
    Ok(Some(Value::Int(n * 2)))
}

fn register(reg: &mut NativeMethodRegistry) {
    reg.register("com/example/Native", "doubleIt", "(I)I", my_native);
}
```

The registry's type signature is at
[`native-api/src/registry.rs:1853`](../native-api/src/registry.rs).
Dynamic `RegisterNatives` from JNI lives on a separate per-class table
inside [`vm/src/native/jni.rs`](../vm/src/native/jni.rs); embedders rarely
need to touch it directly.

## Thread-safety contract

| API | Contract |
|---|---|
| `VmConfig` builders | Setup-only. Never mutate after `Vm::new`. |
| `Vm::invoke` / `Vm::invoke_on_class` | Single-threaded per `Vm`. Each Java thread needs its own `JvmThread`. |
| `SharedVm::class_manager`, `heap`, `monitors`, `thread_registry` | Internally synchronised; safe to read from any thread. Mutating APIs document their lock contract. |
| `SharedVm::native_methods` | Treated as immutable after `Vm::new` returns. Register before. |
| `NativeContext` (inside a native) | Pinned to the calling Java thread; do not share across threads. |
| Custom natives | Must be `fn` (not `FnMut`); state goes in `static` / `OnceLock`. |

The full lock hierarchy is documented in
[`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).

## Pitfalls

- **GC safepoints**: long-running native code must yield at safepoints so
  the collector can make progress. Compute-heavy native helpers should
  break work into chunks and call back into the VM (or simply return) on a
  bounded cadence. See `runtime/gc_integration.rs` and the `gpu-offload`
  `SafepointToken` API in [`gc/src/safepoint.rs`](../gc/src/safepoint.rs)
  for the GPU-critical pattern.
- **Signal handling**: CratonVM installs handlers for SIGSEGV/SIGBUS-based
  null-check elision and stack-overflow detection
  ([`vm/src/runtime/signals.rs`](../vm/src/runtime/signals.rs)). Embedders
  that install their own handlers must chain to ours, not replace them.
- **Panic safety**: Rust panics that escape a native into the interpreter
  are caught at the dispatch boundary but the JVM state is left
  *unspecified* — treat them as fatal. Prefer returning `MethodCallFailed`
  (typed Java exception) over panicking.
- **`unwrap()` on `MethodCallResult`**: a Java exception thrown out of an
  invocation surfaces as `Err(MethodCallFailed)`. Embedders should match on
  the error and either rethrow, log, or convert to a Rust error.
- **Multiple `Vm` instances in one process**: supported but uncommon; the
  global `cratonvm-native-io` sandbox roots and a handful of
  `OnceLock<&mut Vm>` hooks are process-wide. Spawning many short-lived
  VMs is not a tested pattern.

## Further reading

- API surface: [`vm/src/lib.rs`](../vm/src/lib.rs) re-exports the public
  symbols (`Vm`, `SharedVm`, `VmConfig`, `MethodCallFailed`, `JvmThread`).
- Bootstrap reference: [`vm-cli/src/main.rs`](../vm-cli/src/main.rs).
- Architecture overview: [`ARCHITECTURE.md`](../ARCHITECTURE.md).
- Flag reference: [`docs/CONFIG.md`](CONFIG.md).
- GC sizing and backends: [`docs/gc-tuning.md`](gc-tuning.md).
- Platform feature matrix for syscalls: [`docs/PLATFORMS.md`](PLATFORMS.md).
- Lock hierarchy: [`vm/src/runtime/lock_order.rs`](../vm/src/runtime/lock_order.rs).
