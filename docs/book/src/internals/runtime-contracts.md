# Runtime Contracts

CratonVM crosses unsafe and concurrent boundaries: generated machine code,
moving objects, native functions, loader namespaces, and stop-the-world
coordination. These contracts are the rules that keep those boundaries
composable. Treat them as review invariants.

## Dependency ownership

Lower layers expose data and capability interfaces; the VM composes them.

- `reader` parses bytes and does not depend on the runtime.
- shared identities and value/object types live in `types`.
- compiled-artifact contracts live in `jit-api`, so class loading does not
  import the concrete JIT.
- native implementations depend on `native-api`, not on `vm`.
- the collector consumes registered root providers; it does not import a Java
  library overlay.

If a lower-level crate needs a higher-level concrete type, first look for a
missing API or ownership inversion.

## Class identity and initialization

Class identity includes the defining loader. Caches and native/reflection
operations must carry loader or declaring-class identity rather than looking up
by binary name in a global namespace.

Initialization has a Java-visible state machine. Allocation or static access
must not expose a class as initialized until `<clinit>` completes successfully.
The shared allocation lowering calls the runtime contract that enforces this
state; a tier must not replace it with an unchecked heap bump.

## Object-reference ownership

`ObjectRef` is an address-like value, so every long-lived owner outside a
precisely scanned Java frame must participate in root management.

- interpreter frames publish typed reference slots;
- JIT frames publish maps and safepoint state;
- JNI/native critical regions pin or handle objects;
- VM/native side tables register scan and remap callbacks; and
- collectors obtain external roots only through the registry.

For a moving collection, finding a reference is not enough: every owner must
also rewrite it. New side tables containing `ObjectRef` require paired scan and
remap coverage plus a relocation test.

## Write barriers and publication

Every reference store into the heap must use the barrier-correct store path.
This includes interpreter stores, JIT inline stores, reflection, JNI/native
stores, array copies, and deserialization.

Allocation publishes a fully initialized header and layout. A compiled fast
path may bump a TLAB directly only while it preserves the same header,
zeroing/reference, class-initialization, and failure semantics as the runtime
helper.

## Safepoints and compiled frames

Before calling a helper that can allocate, block, safepoint, or transfer
control, compiled code must make live Java state discoverable. Stack-map
metadata and frame publication must agree with register allocation and spill
locations.

Precise maps are an optimization only when the fallback scanner and
pre-safepoint spill contract remain sound. Moving collection requires a
complete per-cycle proof; uncertainty diverts to non-moving collection.

## Exception signaling

The JIT uses `i64::MIN` as a return sentinel for a published exception/deopt
signal. Because a Java `long` can legitimately equal that bit pattern, compiled
call sites must consult the out-of-band signal before bailing. Never infer an
exception from the return bits alone.

Exception handlers require the locals and operand stack for the throwing bytecode
site. Admission is fail-closed: if a backend cannot publish a precise handler
frame for a protected throwing shape, it must decline compilation or retain an
explicit deoptimization edge.

Dead locals in a reconstructed snapshot are `Undefined`. Reading an
uninitialized machine home as an object fabricates a root and can corrupt both
exception handling and GC.

## Dispatch publication

Virtual/interface call-site state moves from monomorphic to a four-entry PIC and
then to a compact hashed tail. The hashed tail is eight sets by two ways and is
published atomically.

Generated code and Rust layout constants form one ABI:

- class-id table offset;
- entry-pointer table offset;
- context-bit table offset;
- set/way counts; and
- hash shift/multiplier.

Offset tests must fail if either side changes. Entry pointers require strong
owners for as long as generated code can load them.

## Monitor semantics

Uncontended monitor enter/exit uses thin-lock mark-word operations. Contention,
recursion, wait/notify, or inflation enters runtime coordination.

A compiler may elide a monitor only with proof for that exact bytecode PC and
object. Evidence that some other allocation was scalar-replaced is not a lock
elision proof. Monitor-only compiled methods still need the thread/TLS context
required by the helper.

## Native capability boundary

Native methods operate through narrow capability traits composed by
`NativeContext`: heap, class, invoke, thread, system, exception, and related
operations. A method should request the smallest capability surface practical.

The ordinary native-call path keeps up to eight argument/pin slots inline.
Larger signatures use a correct heap fallback. Optimizing the common envelope
must not introduce a hard argument-count limit.

Implementation-heavy compatibility domains belong in separate crates.
Registration and Java-object marshalling can remain in the facade crate, while
pure crypto/security kernels compile behind their own package boundaries.

## Configuration lifecycle

Declared VM configuration is parsed once into the typed flag snapshot before
subsystems initialize. A subsystem must not reinterpret the same environment
variable with a different default. New declared flags require:

- type and default;
- owner;
- user-visible documentation;
- whether changing it between VMs in one process is valid; and
- a removal or stabilization condition if experimental.

Application environment variables are not VM flags and retain normal live
environment semantics.

## Bootstrap typestate

Bootstrap transitions are consuming and ordered:

```text
Allocated → ClassesReady → NativesReady → RuntimeReady
```

Do not expose a `finish` or runtime entry point from an earlier state. Add a
boundary invariant and a failing test whenever a new subsystem becomes
required for the next phase.

## Change-review checklist

For a change crossing these boundaries, ask:

- Does it preserve loader-qualified class identity?
- Who owns every retained `ObjectRef`, and how is it scanned/remapped?
- Does every heap reference store execute the correct barrier?
- Can this call allocate, block, safepoint, throw, or deopt?
- Is live JIT state published before that call?
- Can a legitimate return value equal the exception sentinel?
- Are generated offsets/layouts pinned by tests?
- Is lock elision tied to an exact proof?
- Is configuration parsed once?
- Does the crate dependency still point downward?
- Is there an executable regression probe covering the contract?
