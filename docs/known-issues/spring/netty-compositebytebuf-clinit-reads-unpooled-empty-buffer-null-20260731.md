# Netty `CompositeByteBuf.<clinit>` reads `Unpooled.EMPTY_BUFFER` as null — `NoClassDefFoundError` cascade

**Status: OPEN — found 2026-07-31**

## Symptom

`WebServiceMessageSenderFactoryTests.httpWhenDetectedReactor()` fails with a
bare `java.lang.NoClassDefFoundError` (no message in the JUnit summary —
the real cause is only visible in the VM's own stderr trace):

```
JUnit Jupiter:WebServiceMessageSenderFactoryTests:httpWhenDetectedReactor()
    => java.lang.NoClassDefFoundError
```

`.err.log` shows the underlying `<clinit>` failure that produced it:

```
WARN cratonvm_vm::vm::vm_util: <clinit> failed — wrapping in ExceptionInInitializerError class=io/netty/buffer/CompositeByteBuf cause=java/lang/NullPointerException Cannot invoke "io.netty.buffer.ByteBuf.nioBuffer()" because "io.netty.buffer.Unpooled.EMPTY_BUFFER" is null
  [CLINIT-TRACE 0] at SbRunner.main (SbRunner.java:36) bci=133
  [CLINIT-TRACE 1] at org/junit/platform/launcher/core/SessionPerRequestLauncher.execute (SessionPerRequestLauncher.java:67) bci=13
  ... (JUnit platform launcher bootstrap frames)
```

`io.netty.buffer.CompositeByteBuf`'s own `<clinit>` reads the static field
`io.netty.buffer.Unpooled.EMPTY_BUFFER` and gets `null`, then calls
`.nioBuffer()` on it — an NPE inside `<clinit>` that the JVM per JVMS §5.5
wraps as `ExceptionInInitializerError`, which then surfaces as
`NoClassDefFoundError` on every subsequent attempt to use the class (the
one JUnit sees, with no message, because `NoClassDefFoundError`'s own
message is set from the original failed class name only on the *first*
occurrence — subsequent lookups just report the cached failure).

Full logs:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260731/all-jit/logs/module_spring-boot-webservices.org.springframework.boot.webservices.client.WebServiceMessa-46dbb26eef18.{out,err}.log`

## Root cause (mechanism identified, exact trigger not pinned down)

Neither `Unpooled` nor `CompositeByteBuf` has any CratonVM-specific native
shim (`grep -rl "CompositeByteBuf\|Unpooled" native-builtins/src
native-api/src vm/src` finds no hits) — both run as unmodified real Netty
bytecode. For `Unpooled.EMPTY_BUFFER` (a `static final` field assigned in
`Unpooled`'s own `<clinit>`) to read as `null` from a class whose own
`<clinit>` explicitly triggers loading/using `Unpooled`, the JVMS-mandated
per-class initialization lock must be in the **same-thread re-entrant**
state: `Unpooled`'s `<clinit>` must already be running on this thread, and
something in that still-in-progress `<clinit>` — *before* the `EMPTY_BUFFER
= ...` assignment executes — must (directly or transitively) trigger
`CompositeByteBuf`'s class initialization. CratonVM's own class-init state
machine (`vm/src/vm/vm_util.rs`, around the `ClassState::Initializing` +
`initializing_thread` handling, see its doc comment "`Initializing` by the
**same** thread → return immediately (re-entrancy)") implements exactly
this JVMS-correct semantics, so `CompositeByteBuf`'s nested `<clinit>`
proceeds without waiting — and its `GETSTATIC Unpooled.EMPTY_BUFFER`
legitimately observes the field's current (still-null) value.

This is architecturally the same class of bug as the documented
`docs/known-issues/jit-bans` / cross-class `<clinit>` ordering issues: a
circular static-initialization dependency between two classes, where the
specific interleaving depends on exactly which code path first forces
`CompositeByteBuf` to load while `Unpooled` is mid-`<clinit>`. Not
confirmed this session which call inside `Unpooled.<clinit>` (constructing
`EmptyByteBuf(ByteBufAllocator.DEFAULT)`, or resolving the default
allocator itself) ends up touching `CompositeByteBuf` on CratonVM — real
Netty's own `Unpooled`/`EmptyByteBuf` construction path does not obviously
reference `CompositeByteBuf` by inspection of the field list involved, so
it's plausible CratonVM triggers eager class loading/verification (e.g. of
a method signature or field type mentioning `CompositeByteBuf`) at a point
where real HotSpot's lazier resolution would not yet have touched it — this
would make it CratonVM-specific rather than a latent Netty ordering bug
that also affects HotSpot.

## Affected classes

- `module/spring-boot-webservices` — `org.springframework.boot.webservices.client.WebServiceMessageSenderFactoryTests` (`httpWhenDetectedReactor`)

## Suggested next step

Add temporary `CRATONVM`-side tracing of every class-init entry/exit around
`io/netty/buffer/*` during this specific test, or run with
`CRATONVM_SYMBOLIZE=1`/existing clinit trace machinery extended one level
deeper, to capture the full call chain from `Unpooled.<clinit>` down to the
`CompositeByteBuf` trigger. Also worth a quick differential: does plain
HotSpot ever load `CompositeByteBuf` before `Unpooled` finishes for this
same test (it shouldn't, given Netty ships without this bug on every other
JVM) — confirms this is CratonVM-specific ordering, not a latent Netty
issue this harness happens to expose first.
