# `DefaultCatalogAndSchemaTest` — JIT dynamic-proxy dispatch gap raises `AbstractMethodError` on ByteBuddy's `Invoker` proxy — FIXED

**Status: FIXED (2026-08-06).** Root-caused, fixed, and verified with a full
132-method run plus a three-class regression check on the hottest dispatch
path in the VM (`invokevirtual`/`invokeinterface`).

## 1. Symptom

3 of 132 parameterized methods on
`org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
(`enhancedSequenceGenerator`, `enhancedTableGenerator`, `incrementGenerator`)
failed with:

```
java.util.ServiceConfigurationError: org.hibernate.bytecode.spi.BytecodeProvider:
Provider org.hibernate.bytecode.internal.bytebuddy.BytecodeProviderImpl could not be instantiated
Caused by: java.lang.AbstractMethodError: method net/bytebuddy/utility/Invoker.invoke(Ljava/lang/reflect/Method;Ljava/lang/Object;[Ljava/lang/Object;)Ljava/lang/Object; has no Code attribute
	at net.bytebuddy.utility.dispatcher.JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke(JavaDispatcher.java)
	at net.bytebuddy.utility.dispatcher.JavaDispatcher$ProxiedInvocationHandler.invoke(JavaDispatcher.java:1179)
	...
```

`net.bytebuddy.utility.dispatcher.JavaDispatcher` holds a static
`Invoker INVOKER` field built via
`Proxy.newProxyInstance(loader, new Class[]{Invoker.class}, handler)` — `Invoker`
is a plain interface with no real implementation; every instance is a JDK
dynamic proxy. `AbstractMethodError` on `Invoker.invoke` — the interface's own
abstract declaration, with no `Code` attribute — means dispatch resolved
directly to the interface method instead of being forwarded to the proxy's
`InvocationHandler`.

## 2. Root cause

CratonVM's dynamic-proxy support (`vm/src/runtime/proxy.rs`, "WP2.5") gives
every `Proxy.newProxyInstance` instance, regardless of interface set, the same
synthetic runtime class `java/lang/reflect/Proxy$Instance`
(`PROXY_INSTANCE_CLASS`). That class has no real vtable/itable entries for the
interfaces its instances implement — the WP2.5 stop-gap makes each such
receiver a virtual/interface-dispatch special case, intercepted before normal
vtable resolution runs, and forwarded to `proxy_invoke_handler_shared`.

That interception existed in exactly **one** place: the interpreter's
`execute_invoke_kind` (`vm/src/runtime/interpreter/invoke.rs` ~line 1614), via
the `is_proxy_dispatch` check (literal `invoke_class == "Proxy$Instance"` fast
path, `class_chain_reaches_proxy_instance` slow path for subclasses). The
interpreter's own vtable-cache fast path
(`vm/src/runtime/interpreter/dispatch_virtual.rs` ~line 438) independently
forces a cache miss for the same reason, so every interpreted route to a proxy
receiver was covered.

**The JIT's own cache-miss resolver had no equivalent check.** JIT-compiled
`invokevirtual`/`invokeinterface` call sites use a monomorphic inline cache
(`JitMICSlot` in `jit/src/lib.rs`); on a cache miss the generated code calls
into `vm/src/jit/helpers.rs`, which — for `info.invoke_kind` 0 (virtual) / 2
(interface) — resolves the receiver's actual runtime class via
`virtual_dispatch_target_for_receiver` and calls the single general resolver
`crate::vm::invoke_or_native` (`vm/src/vm/vm_exec.rs`) with that class name as
the dispatch class. `invoke_or_native` already special-cases one synthetic
class this same way (`java/lang/annotation/AnnotationProxy`, forwarded to
`annotation_proxy_invoke_shared`) but had **no** case for
`java/lang/reflect/Proxy$Instance`. Grepping `jit/` for
`PROXY_INSTANCE_CLASS`/`Proxy$Instance`/`proxy_invoke_handler`/
`is_proxy_dispatch` returned zero matches — the gap is total, not partial.

So a JIT-compiled call on a proxy receiver fell through to `invoke_or_native`'s
ordinary resolution path, which has nothing to resolve against on the
synthetic `Proxy$Instance` class and lands back on the constant-pool-declared
interface (`Invoker`), finding only its abstract method declaration —
`AbstractMethodError`.

This reproduced specifically on `JavaDispatcher$Dispatcher$ForNonStaticMethod.invoke`
because ByteBuddy calls it on every intercepted reflective operation; it tiers
up to JIT quickly, and once compiled its `invokeinterface` call on `INVOKER`
takes the (buggy) JIT dispatch path instead of the interpreter's. Most other
ByteBuddy-heavy classes in the suite never hit this specific compiled call
site frequently enough, or hit it before compilation — JIT tiering timing is
load-dependent, so "only this class" was never expected to have a single
clean environmental explanation.

## 3. The fix

`vm/src/vm/vm_exec.rs`, `invoke_or_native`: added a proxy-dispatch check
immediately after the existing `AnnotationProxy` check, keyed the same way —
a literal string compare on `effective_class` (the receiver's own resolved
runtime class for a virtual/interface call, never the constant-pool class for
a static call — see `virtual_dispatch_target_for_receiver` in
`vm/src/jit/helpers.rs`):

```rust
if effective_class == crate::runtime::proxy::PROXY_INSTANCE_CLASS {
    if let Some(Value::Object(Some(proxy_ref))) = args.first().copied() {
        if method_name == "getClass" && descriptor == "()Ljava/lang/Class;" {
            let class_id = shared.mem.heap.class_id_of(proxy_ref);
            let mirror = super::get_or_create_class_mirror(shared, class_id);
            return Ok(Some(Value::Object(Some(mirror))));
        }
        return proxy_unbox_primitive_return(
            shared,
            descriptor,
            proxy_invoke_handler_shared(
                shared, thread, proxy_ref, method_name, descriptor, &args[1..],
            ),
        );
    }
}
```

This reuses the interpreter's own handler (`proxy_invoke_handler_shared`) and
return-unboxing helper (`proxy_unbox_primitive_return`, already used by the
`AnnotationProxy` branch above it) — both already live in the same file, so no
new crate-layering work was needed (unlike the `PROXY_LAST_INTERFACES_BITS`
case in `native-builtins/src/lib.rs`, which exists for a genuine
reader/writer crate-direction constraint that does not apply here).

**Why the literal compare is correct and safe, without walking the class
chain:**

- `PROXY_INSTANCE_CLASS`'s own doc comment (`vm/src/runtime/proxy.rs`)
  states every proxy, regardless of interface set, currently lands on this
  one class — no per-interface-set `$ProxyN` subclass exists yet (that is the
  not-yet-implemented WP2.5-A). A literal compare therefore covers 100% of today's
  proxies, exactly like the interpreter's own literal fast path.
- Safety against hijacking a **static** call that merely takes a proxy
  instance as an ordinary argument (e.g. `Objects.requireNonNull(proxy)`)
  comes from keying on `effective_class`, not on the type of `args.first()`.
  For `info.invoke_kind == 3` (invokestatic), the JIT helper passes
  `info.class_name` — the constant-pool-declared class — as `class_name`,
  never the value of any argument, so it can never equal `Proxy$Instance`
  here. `invoke_kind == 1` (invokespecial) is routed through a completely
  different helper (`invoke_special_shared[_on_class]`) that never calls
  `invoke_or_native` at all. Only `invoke_kind` 0/2 (virtual/interface) pass a
  receiver-derived `class_name`, exactly mirroring why the interpreter's
  `is_proxy_dispatch` needs no separate `is_static` exclusion.
- Zero additional cost on every non-proxy dispatch: one string compare,
  identical in shape and cost to the pre-existing `AnnotationProxy` check
  immediately above it.

## 4. Verification

Rebuilt `cratonvm.exe` (release, ~7 min). Fresh binary confirmed by mtime
before every run below.

**Primary repro — full class, fixed binary:**

```
@@RESULT org.hibernate.orm.test.boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest
  found=132 started=132 ok=132 failed=0 aborted=0 skipped=0 ms=2139519
```

132/132, **failed=0** (was `failed=3`). Zero occurrences of
`AbstractMethodError`/`ServiceConfigurationError`/`BytecodeProvider` anywhere
in the run's full log (previously present on the 3 named methods every time).
Runtime ~35.7 min, consistent with this class's documented 620–1804 s clean
range plus this run's cold JIT warm-up.

**Regression check — three ByteBuddy/Weld-heavy CDI classes, same binary,
same invocation pattern, one class at a time:**

| class | result |
|---|---|
| `org.hibernate.orm.test.cdi.type.CdiSmokeTests` | `found=1 started=1 ok=1 failed=0 aborted=0 skipped=0` |
| `org.hibernate.orm.test.cdi.events.standard.StandardCdiSupportTest` | `found=1 started=1 ok=1 failed=0 aborted=0 skipped=0` |
| `org.hibernate.orm.test.jpa.cdi.BasicCdiTest` | `found=1 started=1 ok=1 failed=0 aborted=0 skipped=0` |

All three pass exactly as they did before this change — no regression on the
hottest dispatch path in the VM.

**VM/JIT fast regression suites:**

```
cargo test --release -p cratonvm-vm --lib -- proxy
  test result: ok. 20 passed; 0 failed; 0 ignored; 2505 filtered out

cargo test --release -p cratonvm-jit --lib
  test result: ok. 1954 passed; 0 failed; 1 ignored; 0 measured
```

Both fully green; no new failures introduced.

## 5. Files changed

- `vm/src/vm/vm_exec.rs` — `invoke_or_native`: added the
  `Proxy$Instance`-dispatch branch described in §3, directly below the
  pre-existing `AnnotationProxy` branch.

## 6. Relationship to this class's other, unrelated defects

This class has an unrelated multi-defect history recorded in
`../../../known-issues/hibernate/README.md` (a harness timeout-floor gap, and
four distinct GC old-gen mark/sweep/compact defects — see
`qualfiedtablenaming-runner-timeout-floor-lost-20260731-FIXED.md`,
`gc-overhead-limit-spurious-oom-at-half-full-heap-20260731-FIXED.md`,
`invocation12-late-phase-instability-movable-jit-root-20260801-FIXED.md`,
`map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`). This is a
**fifth, independent defect** — a JIT dynamic-proxy dispatch gap, nothing to
do with GC — found the same way those were closed: read the actual failing
call chain, root-cause it against the interpreter's already-correct behavior,
and fix the gap rather than the symptom. Per that README's own caution, this
class has "stopped discriminating" for the GC-family fixes (it now passes
`132/132` on `dev` tip even with the un-fixed collector gate present), so this
run's clean 132/132 is evidence for *this* fix specifically, not a re-check of
the GC fixes.

## 7. What this predicts elsewhere

Any JDK dynamic proxy (`Proxy.newProxyInstance`) whose `invoke()`-calling call
site is hot enough to JIT-compile was affected — not specific to ByteBuddy or
to Hibernate. The mechanism required no `--nojit` control to distinguish it
(interpreted calls never reached the buggy path at all), and it required no
class-list/environmental explanation beyond "this call site got compiled and
another didn't" — consistent with the working hypothesis recorded before this
investigation began.
