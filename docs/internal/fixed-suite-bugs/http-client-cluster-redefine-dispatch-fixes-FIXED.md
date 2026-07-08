# http.client bug cluster (12 classes) — fixes landed [FIXED, retrospective]

Status: the bulk of this investigation is FIXED and merged to `dev`. This
doc is the historical fix record. Remaining OPEN residuals from this
cluster now have their own focused docs (see `docs/known-issues/README.md`);
this doc is kept in `docs/internal/` per the known-issues convention (a
multi-part investigation moves out once nothing OPEN remains attached to
*this* doc specifically).

## Summary

Investigated across several 2026-07 sessions on the Azure Linux host,
iterating against both a fast Linux+OpenJDK21 loop and the official
Windows+JDK25 harness (`apps/spring-suite-runner`). Of the original 12
target classes: 6 fully fixed outright, 1 (`ReactorClientHttpRequestFactoryTests`)
later retired after current-dev real-JDK probes passed 10/10 (see
`http-client-reactor-windows-timeout-linux-epoll-gap-FIXED.md`), 1
(`reactive.ReactorClientHttpConnectorTests`) fully fixed on Windows, and 4
remaining classes have open residuals now tracked in their own docs (see
below). A separately-discovered AB-BA lock-order deadlock that caused an
intermittent teardown hang in `HttpComponentsClientHttpRequestFactoryTests`
is also FIXED (though a related writer-starvation residual remains open,
tracked separately).

## Root causes fixed

### Round 1 — the core dispatch bug + initial JDK 21 real-mode gaps

1. **Inline-cache population never checked whether an ancestor class had
   been JVMTI-redefined.** Mockito's inline mock maker mocks a *concrete*
   class (e.g. `java.net.HttpURLConnection`) by redefining that class
   directly and instantiating a marker subclass that does **not** itself
   override every mockable method. So the receiver is never redefined —
   only an ancestor is — and calls silently bypassed the mock's advice to
   run CratonVM's real native instead (surfacing as e.g.
   `IllegalArgumentException: HttpURLConnection: URL not set` from inside
   `given()`/`verify()` calls). Fixed in both
   `populate_virtual_invoke_cache` and `try_stackless_invoke`'s ancestor
   walks (`execute_invokevirtual_vtable_fast` already had the correct
   guard — the template for the fix). Verified no regression via a
   20-class spring-web sample outside `http.client` — byte-identical
   results with the fix reverted vs. applied.
2. `JavaLangAccess.defineClass` bridge ran real bytecode instead of the
   registered native `cl_define_class_basic`, tripping real
   `ClassLoader.checkName` on an internal-form (slash) name — broke
   `jdk.internal.reflect.ClassDefiner` (Objenesis's default mock
   instantiation strategy).
3. An over-broad `is_prohibited_package_name` guard blocked
   `jdk.internal.reflect.*`, needed for the same `ClassDefiner` path.
4. Missing `JavaLangAccess.getConstantPool`/`start` bridges (ByteBuddy class
   reading during mock-redefine; Jetty's thread-pool/structured-concurrency
   plumbing).
5. Missing JDK 21 `StackWalker.callStackWalk` overload signature (broke
   Mockito's `LocationImpl`, used on every mocked-method invocation).

### Round 2 — found by comparing against the real Windows/JDK25 baseline

6. **`native_bais_read_byte_array` (`InputStream.read(byte[])`) delegated
   via a direct Rust function call instead of `ctx.invoke_virtual`,**
   bypassing the JDK contract (`read(byte[])` must call the receiver's
   real `read(byte[],int,int)` override). For Jetty's
   `InputStreamResponseListener$Input`, the resulting fallback loop closed
   an infinite cycle, blowing the stack —
   `JettyClientHttpRequestFactoryTests`'s reported `StackOverflowError`.
   Fixed by delegating via `ctx.invoke_virtual` — `native-io/src/lib.rs`.
   Verified on Windows/JDK 25: 5/6 → 6/6 (fully OK).
7. Missing `sun/net/dns/ResolverConfigurationImpl.{init0,loadDNSconfig0,notifyAddrChange0}`
   (Windows DNS-config native) cascaded to `NoClassDefFoundError` for
   Netty's DNS provider. Verified on Windows/JDK 25:
   `reactive.ReactorClientHttpConnectorTests` 2/5 → 5/5 (fully OK).
8. Missing `Socket.getSoTimeout()` native override fell through to real
   bytecode's `getImpl().getOption(SO_TIMEOUT)`; CratonVM's synthetic
   `Socket` keeps the host STRING at field slot 0, colliding with where
   real `Socket.impl` sits, so `getOption(int)` threw
   `NoSuchMethodError: java/lang/String.getOption(I)...` — broke Apache
   HttpClient5's `DefaultManagedHttpClientConnection.bind()`. Fixed by
   querying the real underlying `TcpStream`'s read timeout directly —
   `native-builtins/src/net_phase_e.rs`.
9. Missing `JavaLangAccess.join(String,String,String,String[],int)` broke
   several parameterized tests in `HttpComponentsClientHttpRequestFactoryTests`.
10. A chain of Linux-only JDK native gaps (`sun/nio/ch/NativeThread
    .supportPendingSignals0`, `sun/nio/ch/UnixDispatcher.init`, the full
    `jdk/net/LinuxSocketOptions` surface,
    `sun/nio/ch/Net.shouldShutdownWriteBeforeClose0`) needed fixing just to
    reach bug #8 on the Linux dev host — genuinely Linux-only (confirmed
    absent from the Windows JDK 25 install via `javap`), useful for
    continuing to use the Linux host for CratonVM work generally.

### `HashMap`/`HashSet` chain-walk guard gap

`native_map_remove`, `native_map_get`, and `native_map_contains_key`
(`native-collections/src/lib.rs`) walked their bucket chain with no
cycle/length guard, while `native_map_put`/`map_resize_inner`/
`native_map_contains_value` already had one (`CHAIN_WALK_LIMIT = 4096`,
throws `IllegalStateException` instead of spinning forever — see the
existing comments on `put` referencing a past hash-collision-DoS
incident). Added the same guard to the three missing methods. A real,
standalone robustness fix (confirmed via a Linux `gdb` capture of a
`ThreadPoolExecutor` worker stuck inside `HashSet.remove()` during
`processWorkerExit`, racing `interruptIdleWorkers()`) — but empirically did
NOT fix the `HttpComponentsClientHttpRequestFactoryTests` teardown hang
(see next section); kept anyway since it closes a genuine latent gap.

### `class_manager`/`vtable_manager` AB-BA lock-order deadlock (teardown hang)

`HttpComponentsClientHttpRequestFactoryTests` hung intermittently (50-65%
of runs) during `@AfterEach` teardown (`MockWebServer.close()`). Live
`gdb -p <pid> -batch -ex 'thread apply all bt'` captures on an actually-hung
process showed the real mechanism: a classic AB-BA deadlock between two
distinct `parking_lot::RwLock`s, NOT a leaked lock guard as first suspected.

- Class-loading path: `SharedVm::load_class_concurrent`
  (`vm/src/vm/vm_init.rs`) holds `class_manager` (write) for the entire
  `ClassManager::load_class` call, which (via
  `define_class_with_options` → `fire_vtable_install_hook` →
  `vtable::vtable_install_adapter`) acquires `vtable_manager` (write)
  while still holding it.
- Vtable-dispatch path (the bug): `execute_invokevirtual_vtable_fast`
  (`vm/src/runtime/interpreter.rs`) took `vtable_manager` (read) and,
  while still holding it, acquired `class_manager` (read) to look up
  `declaring_name`.

Two threads acquiring the same lock pair in opposite order deadlock
permanently. `vtable_manager` wasn't tracked by `runtime::lock_order`'s
enforced hierarchy, so nothing caught the inversion.

Fix (commit `caa4ee65`/`60e2b20d`, merged to `dev` twice independently —
this exact bug was separately rediscovered and fixed in at least 3
unrelated investigations this same day: this cluster, a WildFly
domain-startup investigation, and the http.server ZeroCopy investigation
below): extract the needed fields (`declaring_class_id`, `is_native`)
while still holding the `vtable_manager` guard, then drop it before
acquiring `class_manager`.

Verified with 20-iteration repro loops: 13/20 (65%) hangs before → 5/20
(25%) after. The residual 5/20 is a **separate, still-open** bug — see
`docs/known-issues/class-manager-rwlock-writer-starvation.md`.

## Residuals moved to their own docs

- [`docs/known-issues/class-manager-rwlock-writer-starvation.md`](../../known-issues/class-manager-rwlock-writer-starvation.md) — the residual 25% hang rate above; `parking_lot::RwLock`'s non-fair mode lets readers starve a queued writer.
- [`docs/known-issues/http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md`](../../known-issues/http-client-simpleclienthttpresponsetests-mockito-dispatch-bugs.md) — `SimpleClientHttpResponseTests`'s `UnfinishedVerificationException` + intermittent `(class, method, descriptor)`-substituting `NoSuchMethodError`.
- [`docs/known-issues/spring-web-flow-outputstreamwriter-close-corruption.md`](../../known-issues/spring-web-flow-outputstreamwriter-close-corruption.md) — 2 of the 4 "genuine hang" classes (`OutputStreamPublisherTests`, `SubscriberInputStreamTests`); the other 2 (`JdkClientHttpRequestFactoryTests`, `reactive.ClientHttpConnectorTests`) are flagged there as needing separate investigation.
- [`http-client-reactor-windows-timeout-linux-epoll-gap-FIXED.md`](http-client-reactor-windows-timeout-linux-epoll-gap-FIXED.md) — `ReactorClientHttpRequestFactoryTests`'s residual was later retired; current `dev` passes the class 10/10 under the Azure real-JDK Spring probe.

`SimpleClientHttpRequestFactoryTests`'s residual 4 failures
(`prepareConnectionWithRequestBody`, `deleteWithoutBodyDoesNotRaiseException`,
`httpMethods`, `interceptor`) are pre-existing, out-of-scope synthetic-
`HttpURLConnection` gaps unrelated to this cluster (memory
`huc-setdooutput-wrong-slot-drops-post-body`) — not re-documented here.

## Reproduction

```bash
# Azure host, fresh worktree off dev
CP="$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)"
KRUN_STACK=1 ./target/release/cratonvm --java-home /data/data/jdk25-real \
  -cp "/data/data/spring-suite-runner-shared:$CP" \
  KRun org.springframework.http.client.<ClassName>

# Windows, apps/spring-suite-runner, official harness (JDK 25):
cd apps/spring-suite-runner
CRATONVM_BIN=<your built .exe> KRUN_STACK=1 \
  ./run-suite.sh run --jdk real --jit on --batch 1 --only 'http\.client\.'
```
