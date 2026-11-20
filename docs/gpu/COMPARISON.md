# GPU execution scope

CratonVM is an independent JVM with optional GPU acceleration for a restricted
set of pure static array kernels. It executes authentic OpenJDK class libraries
and uses the compatible policy as an optimization layer. First-party code is
owned by [Craton Software Company](https://craton.com.ar) under GPLv2 with the
Classpath Exception.

The optional backends are CUDA, HIP, and OpenCL. Host compilation, driver
availability, supported lowering, semantic checksums, and useful device
throughput are separate requirements. Unsupported methods execute on the CPU.
See the [GPU reference](README.md), [HIP](hip-backend.md),
[OpenCL](opencl-backend.md), and [hardware checks](ci.md).

Other projects use different execution models and support matrices. Consult
their own maintained documentation when comparing features or licensing:
[TornadoVM](https://github.com/beehive-lab/TornadoVM),
[Project Babylon](https://openjdk.org/projects/babylon/),
[Aparapi](https://github.com/Syncleus/aparapi), and
[IBM Semeru](https://developer.ibm.com/languages/java/semeru-runtimes/).
