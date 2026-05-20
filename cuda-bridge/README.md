# cuda-bridge

Thin CUDA Driver API bridge for CratonVM GPU offload. **No JVM-specific
code lives here** — only device discovery, module loading, memory
allocation, memcpy, and kernel launch.

## Build modes

| Cargo features  | What you get                                                                 |
| --------------- | ---------------------------------------------------------------------------- |
| _none_          | Crate compiles; every entry point returns `DeviceError::NoDriver`.           |
| `cuda`          | Real driver bindings via `cudarc`. Requires CUDA Toolkit 12.x.               |
| `gpu-it`        | Enables `cuda` plus tests that launch real kernels (need an attached GPU).   |

## CUDA toolkit version

The `cudarc` dependency is pinned to `cuda-12060`. If your locally
installed driver is on a different CUDA Toolkit major version, update
the feature flag in [`Cargo.toml`](Cargo.toml) and re-run `cargo build
--features cuda`. The cudarc crate gates the FFI bindings by these
feature flags, so a mismatch produces a clear compile-time error rather
than a runtime crash.

## Usage sketch

```ignore
let ctx = cuda_bridge::DeviceContext::new(0)?;
let module = cuda_bridge::DeviceModule::from_ptx(&ctx, PTX, &["vector_add"])?;
let a = cuda_bridge::DeviceBuffer::from_host(&ctx, &[1i32, 2, 3, 4])?;
let b = cuda_bridge::DeviceBuffer::from_host(&ctx, &[10i32, 20, 30, 40])?;
let mut out = cuda_bridge::DeviceBuffer::<i32>::zeros(&ctx, 4)?;
let cfg = cuda_bridge::LaunchConfig::elementwise(4);
module.launch(&ctx, "vector_add", &cfg, (&a, &b, &mut out, 4i32))?;
let mut host = vec![0i32; 4];
out.to_host(&mut host)?;
assert_eq!(host, vec![11, 22, 33, 44]);
```
