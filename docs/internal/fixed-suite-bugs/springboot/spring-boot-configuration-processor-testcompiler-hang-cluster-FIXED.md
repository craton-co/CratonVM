# Spring Boot configuration-processor `TestCompiler` timeout cluster - FIXED

**Status: FIXED 2026-07-18. Severity: MEDIUM.**

The seven affected classes in
`configuration-metadata/spring-boot-configuration-processor` were reported as
HANG after the suite's ordinary 300-second per-class deadline. They repeatedly
use Spring's `TestCompiler`, which invokes the real JDK `javac` in-process for
many fixture sources. This is a CPU-bound, initially silent workload: JUnit
does not print its summary until all of those compilations have completed.

## What was ruled out

The recurring `gc::guard` warning from the original report is benign. During
the reproduction one CPU core stayed busy rather than a test thread parking or
deadlocking. Two setup errors were also excluded before interpreting test
results: an old Windows-formatted generated classpath caused a class-loading
failure, and launching from the Spring Boot root rather than the Gradle module
root caused fixture files to be absent. The final reproductions regenerated the
Linux classpath and used the module working directory selected by the suite
runner.

## Residual discovered after allowing natural completion

On the pre-fix JIT build,
`ConfigurationMetadataAnnotationProcessorTests` completed naturally in
629.68 seconds but one of its 65 tests failed. The real JDK 25 compiler threw
`AssertionError: FileManager initialization error` from `ClassReader`, where
its `Context` no longer contained the `JavaFileManager`. The exact same run
under `--nojit` completed all 65 tests successfully in 680.00 seconds.

The failure was isolated to tiered compilation of the real JDK method
`com/sun/tools/javac/api/JavacTool.getTask`. That method creates the compiler
`Context` and installs its `JavaFileManager`; excluding only this method from
JIT compilation made the JIT run pass naturally in 626.39 seconds. This is a
JIT correctness issue, not a Spring fixture or classpath failure.

## Fix

`vm/src/jit/skip_list.rs` permanently and unconditionally keeps
`JavacTool.getTask` interpreted, including when broad JIT policies are enabled.
The method is cold compiler setup relative to application execution, so this
is a bounded fail-closed mitigation while its lowering defect remains
unproven. A unit test asserts the exclusion under both conservative and
aggressive policies.

`apps/spring-boot-suite-runner/run-spring-boot-suite.ps1` now gives all seven
configuration-processor `TestCompiler` classes a 1200-second deadline. This
preserves the 300-second default for ordinary tests while allowing the runner
to report actual completion or test failure instead of a false HANG.

## Affected classes

* `ConfigurationMetadataAnnotationProcessorTests`
* `ConstructorParameterPropertyDescriptorTests`
* `EndpointMetadataGenerationTests`
* `JavaBeanPropertyDescriptorTests`
* `LombokPropertyDescriptorTests`
* `MergeMetadataGenerationTests`
* `PropertyDescriptorResolverTests`

## Validation

* `cargo test -p cratonvm-vm
  jit::skip_list::tests::javac_tool_get_task_is_unconditionally_interpreted
  --lib` passed on Azure.
* The optimized, task-unique CratonVM binary was rebuilt before the final
  JIT cluster validation.
* The JIT and `--nojit` natural-completion evidence above uses the real Spring
  Boot fixture, regenerated module test classpath, real JDK 25, and the module
  working directory.
* Final JIT validation of the rebuilt binary passed every affected class:
  `ConfigurationMetadataAnnotationProcessorTests` (65 tests, 625.12s),
  `ConstructorParameterPropertyDescriptorTests` (14, 201.05s),
  `EndpointMetadataGenerationTests` (14, 212.28s),
  `JavaBeanPropertyDescriptorTests` (17, 262.13s),
  `LombokPropertyDescriptorTests` (21, 325.11s),
  `MergeMetadataGenerationTests` (14, 218.68s), and
  `PropertyDescriptorResolverTests` (16, 313.75s).

This file supersedes the resolved source report from `docs/known-issues`.
