# Spring SpelCompilerTests mixed-mode timeout

Status: unresolved as of 2026-07-09.

Class: `org.springframework.expression.spel.standard.SpelCompilerTests`

Observed on CratonVM real-JDK JIT with the misc/small worktree binary after the websocket and multipart fixes. The first two test methods complete quickly, but `changingRegisteredVariableTypeDoesNotResultInFailureInMixedMode()` runs a parallel `IntStream.rangeClosed(1, 1_000_000)` over a shared SpEL expression/evaluation context and exceeds the suite timeout in normal JIT mode.

Useful observations:

- `--nojit` completes the class: 3/3 passed in about 518 seconds.
- `CRATONVM_JIT_BISECT_ONLY=java/util/stream/,java/util/concurrent/,java/util/function/` completes the class: 3/3 passed in about 580 seconds.
- A reduced local probe with the same mixed-mode loop completes 100,000 iterations in roughly 42-47 seconds depending on JIT filter, while 1,000,000 iterations exceeds short probe timeouts.
- A broad skip of `org/springframework/expression/` and generated `spel/` classes did not make the real class complete under timeout, so it was not kept.

Likely area: JIT performance/pathological contention in the parallel stream plus SpEL mixed-mode generated expression path, rather than a functional assertion failure.
