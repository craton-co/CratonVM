# 4-Way Application Benchmark Results: CVM vs HotSpot vs GraalVM

**Host**: Ubuntu 26.10 (Linux 7.3.0-9-generic x86_64, 64.177.43.150)

**Date**: 2026-10-31 02:48:16 UTC

## Configurations
[APP_BENCHMARKS_4WAY_RESULTS.md](APP_BENCHMARKS_4WAY_RESULTS.md)
| Arm ID | Runtime | Options |
|---|---|---|
| `cvm_default` | CratonVM 0.3.0 | `--compatible` |
| `cvm_fastboot` | CratonVM 0.3.0 | `--fast-boot` |
| `hotspot` | OpenJDK 25 HotSpot | `-Xmx4g` |
| `graalvm` | Oracle GraalVM 25 | `-Xmx4g` |

## Results Summary

| Benchmark Suite | Description | CVM Default (ms) | CVM Fast-Boot (ms) | HotSpot (ms) | GraalVM (ms) | Status |
|---|---|---|---|---|---|---|
| **CommonsMath-NeuralNet** | Kohonen SOM, 2D mesh, neuron strings, decay functions (66 tests) | 2,464 | 2,450 | 436 | 498 | `66 tests ok` |
| **CommonsMath-Transform-FFT** | Fast Fourier Transform (Cooley-Tukey, complex transforms, 10 tests) | 2,107 | 2,158 | 407 | 457 | `10 tests ok` |
| **CommonsMath-LeastSquares** | Gauss-Newton least squares & Cholesky decomposition (21 tests) | 2,221 | 2,229 | 426 | 493 | `21 tests ok` |
| **BouncyCastle-EC-Math** | Elliptic curve arithmetic, GF(2^m) polynomials & secp curves (23 tests) | 313,062 | 311,445 | 56,753 | 57,956 | `OK (23 tests)` |

## Detailed Round Breakdown (ms)

### CommonsMath-NeuralNet

| Round | CVM Default | CVM Fast-Boot | HotSpot | GraalVM |
|---|---|---|---|---|
| Round 1 | 2,516 ms | 2,479 ms | 436 ms | 509 ms |
| Round 2 | 2,694 ms | 2,496 ms | 448 ms | 506 ms |
| Round 3 | 2,464 ms | 2,450 ms | 449 ms | 498 ms |

### CommonsMath-Transform-FFT

| Round | CVM Default | CVM Fast-Boot | HotSpot | GraalVM |
|---|---|---|---|---|
| Round 1 | 2,120 ms | 2,198 ms | 407 ms | 461 ms |
| Round 2 | 2,237 ms | 2,158 ms | 410 ms | 485 ms |
| Round 3 | 2,107 ms | 2,240 ms | 408 ms | 457 ms |

### CommonsMath-LeastSquares

| Round | CVM Default | CVM Fast-Boot | HotSpot | GraalVM |
|---|---|---|---|---|
| Round 1 | 2,392 ms | 2,229 ms | 437 ms | 493 ms |
| Round 2 | 2,221 ms | 2,403 ms | 426 ms | 504 ms |
| Round 3 | 2,398 ms | 2,418 ms | 430 ms | 501 ms |

### BouncyCastle-EC-Math

| Round | CVM Default | CVM Fast-Boot | HotSpot | GraalVM |
|---|---|---|---|---|
| Round 1 | 313,062 ms | 311,445 ms | 56,753 ms | 57,956 ms |
