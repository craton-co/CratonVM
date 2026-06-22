# The Interpreter

The interpreter is CratonVM's baseline execution engine. Every method runs here
first; hot methods are later handed to the [JIT compiler](jit.md). It lives in
the `cratonvm-vm` crate under `vm/src/runtime/`.

## The dispatch loop

The core is a single dispatch loop that matches on the current bytecode and
executes it. CratonVM implements **140+ fast-path opcodes** directly in the
loop: each reads its operands, manipulates the operand stack and local
variables, and advances the program counter.

Key runtime modules:

| Module | Responsibility |
|--------|----------------|
| `interpreter.rs` | The main dispatch loop and opcode implementations. |
| `frame.rs` | A stack frame: local variables + operand stack. |
| `call_stack.rs` | The per-thread chain of frames. |
| `value_stack.rs` | The typed operand stack. |
| `exceptions.rs` | Java exception creation and throw handling. |
| `invokedynamic.rs` | Lambda / method-reference bootstrap via `LambdaMetafactory`. |

## The value representation

Operands and locals are typed values. Rather than storing a tagged enum per slot
(payload + tag interleaved), CratonVM uses a **Structure-of-Arrays** layout: one
array holds the raw 64-bit payloads and a parallel array holds the type tags.
This is more cache-friendly and lets the garbage collector scan a frame for
object references by walking the tag array — it knows exactly which slots hold
references without parsing values.

## Frames and the call stack

Each method activation gets a `Frame` with its local-variable array and operand
stack (sized from the method's `max_locals` / `max_stack`). Frames are chained on
a per-thread call stack. The maximum depth is bounded (default 1024 frames,
configurable via `RJ_MAX_STACK_DEPTH`); exceeding it raises
`StackOverflowError`.

## The two-layer exception model

Method calls can fail two fundamentally different ways, and CratonVM keeps them
separate:

- **A Java-catchable exception** — an ordinary `Throwable` the program can catch.
  The interpreter unwinds to the nearest matching catch/finally handler.
- **An internal VM error** — a bug or unsupported operation in the VM itself.
  This is *not* a catchable Java exception; conflating the two would let VM bugs
  silently masquerade as program-level exceptions.

This separation is one of the project's core design decisions (see
[Architecture Overview](architecture.md)).

## Intrinsics

The interpreter can install **intrinsic** inline-cache entries for hot,
well-known methods, short-cutting ordinary native/bytecode dispatch. These can
be disabled for differential testing with `CRATONVM_DISABLE_INTRINSICS`.

## Invokedynamic and lambdas

`invokedynamic` call sites are bootstrapped on first execution. Lambda and
method-reference sites go through `LambdaMetafactory`; libraries that supply
their own bootstrap methods are also supported, so frameworks that build custom
call sites link and run.

## Handing off to the JIT

Each method has an invocation counter. When it crosses the warmup threshold (see
[The JIT Compiler](../user-guide/jit-compiler.md)), the method is compiled and
subsequent calls dispatch to native code. Long-running loops are picked up
mid-method via On-Stack Replacement, transferring interpreter locals into the
compiled frame.
