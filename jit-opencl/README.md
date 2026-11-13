# cratonvm-jit-opencl

Kernel-IR to OpenCL C rendering for CratonVM GPU offload.

Part of the [CratonVM](https://github.com/craton-co/cratonvm) Java Virtual Machine.

## Overview

This crate provides the OpenCL C lowering target for CratonVM's GPU offloading subsystem.
It takes the target-neutral Kernel IR (`kir`) produced by `jit-cuda` and renders it into standards-compliant OpenCL C (compatible with OpenCL 1.2, 2.0, and 3.0 devices).

## Key Characteristics

- **Zero runtime dependencies on OpenCL SDK at build time**: builds anywhere without OpenCL headers or SDKs.
- **Strict IEEE-754 semantics**: explicit handling of bitcasts (`as_float`, `as_uint`, etc.), float-to-integer conversions with saturation and NaN-to-zero (JLS §5.1.3), and branch-free divisions.
- **Global Memory Qualifiers**: correctly tracks and qualifies memory buffers with `__global`.
- **Refusal over guesswork**: unlifted instructions (`Instr::Raw`) are refused explicitly to prevent incorrect kernel execution.
