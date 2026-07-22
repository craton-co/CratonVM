# `MicrometerTracingAutoConfigurationTests`: filtered classpath condition honored

**Status: RESOLVED — 2026-07-18.**

The failing test hid `io.micrometer.core` with `FilteredClassLoader` but
received the metrics-aware observation handler group.

## Root cause and resolution

This residual called `ClassUtils.isPresent(name, null)`. CratonVM's native
`spring_class_utils_for_name_impl` treated null as a VM-global lookup, which
made `MeterRegistry` visible despite the context runner's thread context
`FilteredClassLoader`. The native now mirrors Spring's Java implementation:
null resolves through `Thread.currentThread().getContextClassLoader()` before
user-loader dispatch.

A focused probe verifies that the hidden class is absent through both
`ClassUtils.forName(name, null)` and `ClassUtils.isPresent(name, null)`. The
11-test class passed in JIT and `--nojit` closure runs on 2026-07-18.
