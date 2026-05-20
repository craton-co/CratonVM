# CratonVM — Prebuilt Distribution

This directory contains prebuilt binaries and example programs.

For full documentation, see the [project README](../README.md).

## Quick Start

```bash
cratonvm --classpath examples HelloWorld
```

## Examples

| File | Description |
|------|-------------|
| `examples/HelloWorld.java` | Basic Hello World |
| `examples/ArithmeticTest.java` | Arithmetic operations |
| `examples/ControlFlow.java` | If/else, loops, switch |
| `examples/ExceptionTest.java` | Try/catch/finally |
| `examples/StringTest.java` | String operations |

Compile examples with `javac examples/*.java`, then run with `cratonvm --classpath examples <ClassName>`.
