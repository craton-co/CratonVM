# Problem 4: a Rust copy of the class library stands in front of the bytecode

Filed 2026-11-11 from code analysis only. Measured on the tree at `39e1145b06` (`dev` of 2026-11-01). Counts are regex counts over `*.rs`
and are approximate.

## Problem

The VM runs real JDK bytecode, and it also ships about 1.4 million lines of
Rust that reimplement JDK and third-party library methods. Which one runs for
a given call is decided by a policy spread over several files, keyed by class
name strings, and partly by ambient state. The Rust side keeps its own object
state outside the Java heap. The VM core names specific applications.

## Evidence

- Size: `native-builtins` 1,150,082 lines, `native-collections` 121,256,
  `native-io` 113,082, `native-api` 99,708. That is more than the `vm` crate.
- About **9,833** `register(` calls. The kind of a registration is ambient:
  1,316 `set_category(` calls and 106 `with_category(` calls set a mutable
  `current_category` (`native-api/src/registry.rs:14137`), and
  `registry.rs:17458` falls back to `SyntheticStub` when none is set.
- The dispatch decision is not in one place.
  `vm/src/runtime/interpreter/native_override.rs` (10,980 lines) has
  `force_native_over_real_jdk_bytecode` plus 48 `is_*_native_override`
  predicates; its header says "a registered native wins over real bytecode
  unconditionally". `resolve_dispatch` (`vm/src/vm/vm_exec.rs:1368`) is the
  intended single resolver and has 27 call sites. A comment at
  `native_override.rs:40` notes two gates had to be kept in step by hand.
- Rust-side object state: about 371 maps keyed by object reference or address
  in `native-builtins`, 192 in `vm`. Each is a root set the collectors must
  know about and remap. `native-api/src/retired_shadow.rs` (45,595 lines)
  records why shadows cannot simply be switched off: "the surviving bridges
  ARE the object model for large parts of `java.base`".
- The core names applications. 328 non-comment, non-test code lines in
  `vm/src`, `jit/src` and `classloading/src` contain a Spring, Hibernate,
  JBoss, Quarkus, Netty, H2, Tomcat or WildFly class name. Examples:
  - `native_override.rs:1049`: a native replaces H2's `ValueBigint.get(long)`
    to extend its cache from 100 to 1000 entries;
  - `classloading/src/class_manager.rs:25912`: a synthetic field layout for
    `io/quarkus/bootstrap/runner/RunnerClassLoader`;
  - `vm/src/runtime/interpreter/invoke.rs:1720`: a branch on a Spring
    annotation class in the invoke path.
- 929 `COMPAT` switches and 756 mode checks (`jdk_only` / strict) keep the two
  worlds apart at run time.

## Why it is architectural

- Two implementations of one method must agree byte for byte with HotSpot,
  for every JDK version supported. The Rust copy has no upstream.
- An override keyed by class name applies to any class with that name, in any
  loader, at any version of the library.
- Object state in Rust maps is invisible to Java code, reflection, `Unsafe`,
  serialization and agents, and each map is a GC hazard.
- The ambient kind makes a mistake silent: the registration compiles, runs in
  one mode and vanishes in the other.

## Plan

1. **One resolver.** Make `resolve_dispatch` the only function that answers
   "bytecode or native" and route the interpreter fast paths, the JIT helper
   dispatch and reflection through it. Cache its answer on the resolved
   method, not in name-keyed side tables. Delete the 48 predicates by turning
   each into registry data (step 2).
2. **Kind is an argument.** Change `register` to take `NativeKind`
   explicitly and delete `current_category`. The compiler then finds every
   call site. Mechanical: each site passes what `set_category` set above it.
3. **Move application workarounds out of the core.** Introduce one table type
   in the registry: `(class, method, descriptor, library version predicate,
   reason)`. Port the 328 lines to rows. Then burn the table down: each row is
   either a VM bug to fix (most are, by their own comments) or a performance
   shortcut to delete.
4. **Retire shadows by state, not by method.** Continue the per-subsystem
   order `retired_shadow.rs` already uses: a class's state becomes real Java
   fields first, then all its natives go together. Track one number per
   package: Rust-side maps keyed by object. Collections and `java.lang.invoke`
   first, since they have the most maps.
5. **Fix the end state.** What stays in Rust is what HotSpot implements
   natively: the JNI natives of the JDK and a reviewed intrinsic list. Both
   modes then run the same bytecode and the mode switch shrinks to boot
   speed-ups that have no Java-visible effect.

## Done when

- No class name of a third-party library appears in `vm`, `jit`, `gc` or
  `classloading` outside tests.
- `current_category` does not exist.
- Object-keyed maps in `native-builtins` are under 20, each registered as a
  GC root set through one API.
