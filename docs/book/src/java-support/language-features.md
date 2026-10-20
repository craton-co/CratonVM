# Language Features

CratonVM runs bytecode, so it supports whatever a Java compiler emits — the
question is which bytecode shapes and runtime behaviors the VM implements. This
chapter summarizes the Java language features that work. For per-version
highlights see [Java Version Support](version-support.md); for standard-library
classes see [Standard Library Coverage](standard-library.md).

## Core language

- **All primitive types:** `boolean`, `byte`, `char`, `short`, `int`, `long`,
  `float`, `double`.
- **Full arithmetic, bitwise, shift, and comparison operators**, including
  correct integer division/remainder semantics and IEEE-754 floating point.
- **Control flow:** `if`/`else`, `for`, `enhanced for`, `while`, `do`/`while`,
  `switch` (table and lookup), labeled break/continue.
- **Arrays:** primitive and reference, including multi-dimensional arrays.
- **String concatenation** (including the `invokedynamic`/`StringConcatFactory`
  form emitted by modern compilers).
- **Exception handling:** `try`/`catch`/`finally`, `try`-with-resources,
  `throw`, multi-catch, checked and unchecked exceptions.
- **Classes, interfaces, abstract classes, enums**, nested and inner classes.
- **Inheritance, overriding, and `super` calls**; default and static interface
  methods.
- **`static` fields and methods**, static initializers, and class
  initialization ordering.
- **Type operations:** `checkcast`, `instanceof`, autoboxing/unboxing.
- **Generics** (compiled to bytecode with type erasure) and reflective access
  to generic signatures.

## Functional & modern features

- **Lambda expressions and method references** via `invokedynamic` /
  `LambdaMetafactory`.
- **Records** (JEP 395) and **sealed classes/interfaces** (JEP 409).
- **Pattern matching** for `instanceof` and for `switch`, including record
  patterns.
- **Text blocks.**
- **`var`** local-variable type inference (a compile-time feature; transparent
  at the bytecode level).
- **Virtual threads** and structured-concurrency / scoped-value APIs (see
  [Java Version Support](version-support.md)).

## Concurrency

- **`synchronized`** methods and blocks, object monitors, `wait`/`notify`/
  `notifyAll`.
- **`java.util.concurrent`** locks, latches, semaphores, barriers, and
  concurrent collections (see [Standard Library Coverage](standard-library.md)).
- **Threads**, thread-local storage, and the memory-model primitives the JDK
  classes rely on.

> A correct boot JDK matters for threading: when running against a real JDK,
> ensure it is a modern release. Some thread-creation paths in older JDKs behave
> differently.

## Reflection & dynamic features

- **`Class.forName`, `Class` metadata, `Method.invoke`, `Field` access,
  constructors**, and annotation reflection.
- **Dynamic proxies** (`java.lang.reflect.Proxy`).
- **`invokedynamic`** with arbitrary bootstrap methods (so libraries that build
  their own call sites link and run).

Reflection is broad but not exhaustive — some edge cases are unsupported. See
[Known Limitations](limitations.md).

## Exceptions you can catch

All the standard runtime exceptions and errors behave as Java programs expect,
including `NullPointerException`, `ArithmeticException`,
`ArrayIndexOutOfBoundsException`, `ClassCastException`,
`NumberFormatException`, `IllegalArgumentException`,
`UnsupportedOperationException`, and `StackOverflowError`, among many others.
`NullPointerException` messages include helpful code detail (JEP 358) by
default.
