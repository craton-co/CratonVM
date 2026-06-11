# Code Review — native-builtins app-server compat layer (`nb-appserver`)

Reviewer: Fable (Opus) — static review only, no builds run.
Date: 2026-06-10
Scope (~35k LOC across 14 files):
`servlet.rs`, `jboss_module_loader.rs`, `jboss_msc.rs`, `wildfly_core.rs`,
`spring_startup_bootstrap.rs`, `quarkus_staticinit.rs`, `vertx_eventloop.rs`,
`xnio_async.rs`, `xnio_io_thread.rs`, `logmanager.rs`, `jmx.rs`,
`jmx_openmbean.rs`, `graalvm_compat.rs`, `aot.rs`.

---

## Summary

This layer is the single most concentrated source of *forbidden app-specific
synthetic shims* in the project. Several files (`spring_startup_bootstrap.rs`,
`quarkus_staticinit.rs`) are self-tagged `NativeKind::SyntheticStub` and are
wired **unconditionally** into `register_essential_natives` (the default
real-JDK build path — confirmed at `lib.rs:5638`, `:5667`, `:5261`, `:5267`,
`:1657`, `:7020`). They fake framework behavior to mask underlying VM bugs.

The good news: a number of the worst shims have already been *gated off* or
*disabled* (the WildFly synthetic no-op `main` is behind
`CRATONVM_USE_WILDFLY_SYNTH_BYTECODE`; the Spring CCPP `@Configuration`-disabling
no-op is dead code). But several remain live by default:

- **Quarkus `SerializedApplication.read` hardcodes the Keycloak main class**
  (`org.keycloak.quarkus.runtime.KeycloakMain`) into a synthetic object,
  bypassing the real `.dat` bootstrap — the canonical "fake main" shim.
- **Spring `Environment` is fully faked**: `getProperty()` always returns null,
  `containsProperty()` always false, `acceptsProfiles()` always true.
- **WildFly `AsyncFutureTask.await()` forces the boot future to COMPLETE**
  regardless of whether the MSC service container ever reached RUNNING.
- **Netty/Vert.x `NioEventLoop.execute(Runnable)` enqueues a no-op and silently
  drops the Runnable** — work submitted to the event loop never runs.

Separately, `logmanager.rs` has a genuine **GC-safety bug**: it caches
`ObjectRef` as raw `u64` pointers in three process-global maps with no
GC scan/remap hook (unlike `jboss_msc.rs`, which does both), so cached Logger
mirrors go stale after a moving collection.

`jmx.rs` is a model of *honest* sentinels (metrics with no real source return 0
with explicit "FLAGGED-0, not measured" comments) and should NOT be confused
with the forbidden shims. `aot.rs` and `graalvm_compat.rs` are mostly inert,
honest scaffolding.

---

## Bugs

### B1 (critical) — Netty/Vert.x event loop drops submitted Runnables
`vertx_eventloop.rs:1060-1083` (`native_nel_execute`), registered at
`:1195` against `io/netty/channel/nio/NioEventLoop`,
`io/netty/channel/DefaultEventLoop`, `io/netty/util/concurrent/SingleThreadEventExecutor`.
`execute(Runnable)` validates the arg non-null then enqueues
`Box::new(|| {})` — a no-op — instead of the real Runnable (comment at
`:1078`: "we enqueue a no-op stub"). Any Netty/Vert.x code path relying on
`eventLoop.execute()` to run work (almost all of Netty) silently does nothing.
The natives are tagged `Bridge`, not even `SyntheticStub`.

### B2 (critical) — WildFly boot future force-completed without real service startup
`wildfly_core.rs:1364-1440` (`native_async_future_task_await`). When the
bootstrap future is still `WAITING`, the native flips its status to `COMPLETE`
in place (default behavior; opt-out `CRATONVM_AWAIT_NO_SHORTCIRCUIT`). The
comment is explicit: "we don't drive the MSC service graph to RUNNING ... so the
bootstrap future stays in WAITING ... This native replaces the Java await() body
entirely." This fakes a successful WildFly/Keycloak boot when the service
container never actually reached STABLE.

### B3 (high) — Quarkus `SerializedApplication.read` hardcodes Keycloak main class
`quarkus_staticinit.rs:504-556` (`resolve_main_class` /
`native_serialized_application_read`). Returns a synthetic `SerializedApplication`
whose `mainClass` defaults to `"org.keycloak.quarkus.runtime.KeycloakMain"`
(`:513`), bypassing the real `.dat` parse. Masks a real NIO bug
(`DataInputStream.readInt` over `Files.newInputStream` returns drifted values,
documented `:288-290`). Forbidden "fake main" shim.

### B4 (high) — `logmanager.rs` caches ObjectRef as raw u64 with no GC remap
`logmanager.rs:125` (`logger_registry`), `:729` (`jboss_logger_registry`),
singleton slots `:372`/`:706`; reconstructed via `ObjectRef::from_raw` at
`:208-210`. Pointers stored as `obj.as_ptr() as u64` (`:425`, `:509`, `:760`).
There is **no `gc_scan`/`gc_update` function in this file** — contrast
`jboss_msc.rs:1516 gc_scan_msc_service_roots` + `:1531 gc_update_msc_service_refs`.
The safety comment at `:201-207` claims "we never free ... so the address remains
valid," conflating *never freed* with *never relocated*. After a moving young GC
relocates a cached Logger mirror, the cached u64 is stale and `from_raw` yields a
dangling/wrong object. This is the same bug class MEMORY.md documents repeatedly
(classloader GC-root gap, lang_math cache remap, StringBuffer hazard).

### B5 (high) — Spring `Environment` access is entirely faked
`spring_startup_bootstrap.rs:454-535`, registered live via
`getEnvironment`/`createEnvironment` (`:1078-1089`) and the env getters.
`env_get_property_str` → always null (`:455`), `env_contains_property` → always
false (`:472`), `env_accepts_profiles[_arr]` → always true (`:494`, `:499`),
`env_get_active_profiles` → empty. Masks a real
`AbstractEnvironment.<init>` CLDR/resource-bundle NPE (comment `:1069-1072`).
Any Spring app reading config via `Environment` gets wrong/empty answers.

### B6 (medium) — WildFly `ControlledProcessState` transitions no-op'd to hide VarHandle bug
`wildfly_core.rs:1084-1137` (`native_process_state_set_*`). The setters bypass
real bytecode and mutate only the Rust-side `global_model_controller()`, to mask
an `AtomicStampedReference` VarHandle modeling gap (comment `:1064-1067`). Hides a
real VM defect rather than fixing it.

### B7 (medium) — MSC service-start failures silently swallowed
`jboss_msc.rs:1757-1764` (`drive_starts`). A failing service `start()` is
recorded and dropping continues so "a single failed service does not abort the
whole boot." Pragmatic, but a fault-masking behavior that can present a
half-started container as healthy.

### B8 (low) — ByteBuffer relative-read index arithmetic can overflow/panic
`servlet.rs:1344-1392` (`s2_bb_read2/4/8`, `s2_bb_write2/4/8`). `idx + 1..3/7`
on an i32 from Java bytecode can overflow (debug panic / release wrap);
`idx as usize` of a negative idx becomes huge. Downstream `get/set_array_element`
bounds-checks, so impact is limited, but a debug-build panic is reachable.

### B9 (low) — `spawn_worker` panics on thread-spawn failure
`wildfly_core.rs:404-408`. `.expect("failed to spawn EnhancedQueueExecutor worker")`
turns an OS thread-limit/ENOMEM into a process abort rather than a recoverable
RejectedExecutionException-style path.

---

## Vulnerabilities

### V1 (medium) — `ensure_under_root` weakens symlink-escape check on canonicalize failure
`jboss_module_loader.rs:335-350`. Both candidate and root fall back to the
*non-canonical* path when `std::fs::canonicalize` fails
(`.unwrap_or_else(|_| ...to_path_buf())`, `:336-339`). On failure the
`starts_with` prefix check runs against raw paths, so a symlink whose target is
unresolvable (or a not-yet-existing path) escapes the symlink guard. The `..`
traversal is still caught by the defense-in-depth input check at `:381`, so this
is symlink-escape-only, but the documented "defeats symlink attacks" claim is
not fully upheld.

### V2 (low) — Zip entry size trusted for pre-allocation (memory DoS)
`jboss_module_loader.rs:1660`. `Vec::with_capacity(entry.size() as usize)` trusts
the zip central-directory uncompressed-size for a module JAR entry. A crafted JAR
can declare a huge size to force a large up-front allocation. Module JARs are
semi-trusted (on the configured module path), limiting exposure.

### V3 (low) — `graalvm_dump_configs` allows absolute output paths
`graalvm_compat.rs:883-901`. Rejects `..` components and sanitizes filenames
(good), but an *absolute* `output_dir` (e.g. a sensitive system dir) is accepted
and `create_dir_all` + file writes proceed. Path comes from app code calling the
CratonVM diagnostic agent `cratonvm/graalvm/MetadataAgent.dumpConfigs`, so
exposure is limited to opt-in diagnostic use.

---

## Stubs and Unimplemented (forbidden synthetic-stub policy)

Each of these fakes app behavior or returns placeholder values. Severity to
policy noted.

1. `quarkus_staticinit.rs:504/520` — hardcoded Keycloak main class in
   synthetic `SerializedApplication` (see B3). **Live by default.**
2. `spring_startup_bootstrap.rs:454-535` — faked `Environment` getters
   (see B5). **Live by default.**
3. `wildfly_core.rs:1364` — `AsyncFutureTask.await` force-COMPLETE (see B2).
   **Live by default.**
4. `vertx_eventloop.rs:1060` — `execute()` drops Runnable (see B1).
   **Live by default.**
5. `spring_startup_bootstrap.rs:87-141` — `ApplicationStartup`/`StartupStep`
   all-no-op singletons (`getApplicationStartup` always returns global no-op).
   **Live by default.** Benign (metrics-only), but synthetic.
6. `wildfly_core.rs:1084-1137` — `ControlledProcessState` setter no-ops
   (see B6). **Live by default.**
7. `jboss_msc.rs:1264` (`native_lockable_lock_noop`), `:1279`
   (`native_delegating_logger_returns_false`), `:1299`
   (`native_service_logger_greeting_noop`) — MSC clinit/lock no-op shims.
   Pragmatic, but synthetic overrides of real classes.
8. `logmanager.rs:461-468` — `org.jboss.logmanager.LogManager.<init>` no-op'd
   ("short-circuit it to a no-op"), `:513-522` `readConfiguration` no-op'd.
   Faked log-config init.
9. `jboss_module_loader.rs:2504-2548` — synthetic `JDKModuleLogger.<clinit>`
   to sidestep a JDK class-init NPE. Delegates to real `Level` fields, but a
   synthetic clinit override.
10. `aot.rs:124-151` — `AotCacheEntry::new_placeholder` uses bytecode's first
    8 bytes as a "fingerprint" (not a real SHA-256, comment admits it);
    `compiled_code` always empty. The whole AOT cache is non-functional
    scaffolding (honest: `has_compiled_code()` gates lookups so it never
    falsely claims a compiled hit — `aot.rs:1195`).
11. `xnio_io_thread.rs:553-596` — `CHANNEL_DISPATCHER` default is a no-op and
    `set_channel_dispatcher` is **never called anywhere** in the crate, so the
    XNIO I/O-thread selector loop drops every ready-key event
    (`xnio_io_thread.rs:682` calls the no-op). Incomplete wiring (T19.7.d).
12. `jboss_module_loader.rs:1063-1230 / 2492-2498` — synthetic no-op `main`
    classfile builder. **Gated OFF by default** (`:980-987`, opt-in
    `CRATONVM_USE_WILDFLY_SYNTH_BYTECODE`); `native_wildfly_main_noop` is
    dead-code. Latent in the binary.
13. `spring_startup_bootstrap.rs:2642-2652` — `[demo-shim]` no-op that disables
    ALL `@Configuration`/`@Bean` scanning. **Disabled per policy** (`:1744`,
    dead code via `let _ =`). Latent in the binary.
14. `graalvm_compat.rs:1079-1117` — `RuntimeReflection`/`RuntimeSerialization`/
    `RuntimeJNIAccess.register` no-ops. Acceptable (these are genuinely no-ops
    outside a native image) but recorded as synthetic.
15. `servlet.rs:3371-3421` — entire `java.net.http.HttpClient` reimplemented as
    a synthetic (1-field HttpClient, 4-field HttpRequest, 3-field HttpResponse).
    Does real TCP/TLS I/O, but `HttpResponse.headers()` is never parsed
    (`:3576` field 2 = null), and a null request/URI returns a fabricated
    200/empty (`:3437-3445`). Bypasses real JDK `java.net.http` bytecode.

---

## Performance

### P1 — `mps_find_index_by_name` linear scan per property-source op
`spring_startup_bootstrap.rs:276` and callers. O(n) name scan on every
`MutablePropertySources.replace/addBefore/addAfter`; called repeatedly during
Spring boot. Index by name once if this becomes hot.

### P2 — `register_resource_roots` rebuilds a dedup set per call
`jboss_module_loader.rs:1047-1061`. Locks the global `registered_paths()` set and
does `to_string_lossy` for every path on each module resolution. Fine for a
handful of modules, but the per-call string allocation is avoidable.

### P3 — ByteBuffer per-byte array element access
`servlet.rs:1317-1392` — `s2_bb_read4/8` and `s2_bb_remaining_bytes` read one
byte at a time via `get_array_element` (a trait call each). For per-packet codec
paths this is a chatty hot loop; a bulk copy helper would cut trait-dispatch
overhead.

### P4 — `drive_starts` re-locks `service_roots()` per iteration
`jboss_msc.rs:1722-1726`. Each iteration takes the global mutex to fetch one
service. For large service graphs this is repeated lock acquisition; batch or
snapshot the ready set.

### P5 — `redact_credentials` runs on every log line
`wildfly_core.rs:685` (and logmanager log paths). Credential redaction over the
full message string on each log call; if it compiles regexes or does multiple
passes, cache the matcher and short-circuit when no candidate substring present.

---

## Tests

Inventory: ~447 inline `#[test]` functions across the scope, no external
integration tests for these modules (only `native-builtins/tests/aes_gcm_kat.rs`,
out of scope). Per-file `#[test]` counts:

- Well covered (logic-level): `graalvm_compat.rs` (99), `aot.rs` (67),
  `jboss_module_loader.rs` (46 — incl. path-traversal cases, e.g.
  `../../escape.jar` at `:2823`), `jmx_openmbean.rs` (46), `vertx_eventloop.rs`
  (40), `quarkus_staticinit.rs` (26).
- Thin: `wildfly_core.rs` (19), `xnio_async.rs`/`xnio_io_thread.rs`/`jmx.rs`/
  `logmanager.rs` (18 each), `jboss_msc.rs` (15), `servlet.rs` (12).
- Bare smoke only: `spring_startup_bootstrap.rs` (5 — just "registration runs
  without panic" + surface pins; see `:2661-2678`).

Basis for estimate: most tests exercise pure helpers (socket-id allocation,
service-name interning, AOT cache LRU, OpenType type-mapping, classfile-builder
byte layout) and registration-surface pins. They do **not** assert
end-to-end framework semantics, and crucially they cannot catch the forbidden
shims because the shims' "correct" behavior under test is exactly the fake
(e.g. a Spring `Environment` test would assert `getProperty()==null`, locking in
the bug). The single highest-risk bug (B4, logmanager raw-u64 GC staleness) has
zero coverage — no test allocates, forces a moving GC, and re-reads a cached
Logger. The B1 event-loop-drop and B2 await-force-complete behaviors are
similarly untested for their real consequence.

Estimated coverage: **~45%** by line, heavily weighted toward isolated helpers.
It does **not** plausibly reach 85%. To approach that, the most important missing
tests:
- GC-relocation test for `logmanager.rs` logger registries (B4): cache a logger,
  trigger a moving collection (or a remap callback), assert the cached ObjectRef
  still resolves to the same object — and add the missing `gc_scan`/`gc_update`.
- `native_nel_execute` must actually schedule the passed Runnable (B1): assert
  the queued task is the real one, not a no-op.
- `s2_bb_read*/write*` round-trip + negative/overflow `idx` (B8).
- `ensure_under_root` symlink-escape case where `canonicalize` fails (V1).
- WildFly process-state and await-future state machines under a non-completing
  service container (B2/B6).
- HttpClient `headers()` and error-path (null URI) behavior (`servlet.rs` S.3).

---

## Feature Suggestions

1. **Central shim registry / audit gate.** Every `SyntheticStub`-tagged native
   that overrides a real framework class should be enumerable and gated behind a
   single `CRATONVM_ALLOW_APP_SHIMS` env (default OFF for open-source), so a
   clean build fails loudly on the underlying VM bug instead of silently faking
   app success. The `NativeKind::SyntheticStub` tag already exists — wire it to a
   runtime warning + `--dump-native-registry` census line per shim.
2. **Fix the real bugs the shims mask, then delete them.** Priority order:
   AtomicStampedReference VarHandle (unblocks B6), MSC service-graph drive to
   RUNNING (unblocks B2), `.dat` `DataInputStream.readInt` NIO path (unblocks
   B3), `AbstractEnvironment.<init>` CLDR/resource-bundle NPE (unblocks B5).
3. **Real event-loop dispatch.** Wire `native_nel_execute`/`xnio_io_thread`
   dispatcher to actually invoke `Runnable.run()` via `ctx.invoke_virtual`
   (the MSC path already does this at `jboss_msc.rs:1744`), removing B1 and the
   `set_channel_dispatcher`-never-called gap (#11).
4. **GC-root integration helper.** A shared `gc_root_table!` macro that any
   process-global ObjectRef cache opts into (scan + remap), so files like
   `logmanager.rs` cannot reintroduce B4. Model on
   `jboss_msc.rs:1516/1531`.
5. **Real `HttpResponse.headers()` parsing** in `servlet.rs` S.3, and route the
   whole `java.net.http` surface through real JDK bytecode where possible rather
   than the synthetic 3-field response.
6. **Functional AOT or remove the scaffolding.** `aot.rs` is ~2.2k LOC of
   placeholder cache that never produces compiled code. Either wire it to the
   real x64 JIT (emit `compiled_code`) or delete it to reduce open-source
   surface and the "looks implemented but isn't" footprint.

---

## Files: sampled vs fully read

Fully read (key risk regions read end to end):
- `spring_startup_bootstrap.rs` — env getters, registration, CCPP shim,
  bean-factory fix.
- `quarkus_staticinit.rs` — bootstrap runner, `SerializedApplication.read`,
  `RunnerClassLoader.loadClass`, env allowlists.
- `wildfly_core.rs` — process-state shims, `AsyncFutureTask.await`, worker loop,
  registration.
- `vertx_eventloop.rs` — `native_nel_execute`/schedule, registration, class
  constants.
- `logmanager.rs` — registries, `object_from_u64`, addLogger/readConfiguration.
- `jmx.rs` — VMManagementImpl honest-sentinel batch.
- `graalvm_compat.rs` — registration, ImageInfo, dump_configs path handling.
- `aot.rs` — AotCacheEntry, Leyden lookup/store/load.

Sampled (structure-grepped + targeted regions read):
- `servlet.rs` — read S.3 HttpClient + ByteBuffer helpers + socket-addr parse;
  the NIO selector/socket-registry core (most of the file) was structure-grepped
  only.
- `jboss_module_loader.rs` — read path-traversal guard, synthetic-classfile
  builder, JAR entry reader, synthetic-main gating; module.xml parser sampled.
- `jboss_msc.rs` — read `drive_starts`, GC remap unsafe, registration surface;
  the service-graph state machine sampled.
- `jmx_openmbean.rs` — structure-grepped (short-circuit comments reviewed),
  OpenType recursion not deep-read.
- `xnio_async.rs` — structure-grepped (Int(1) returns + test notes).
- `xnio_io_thread.rs` — read dispatcher shim + event loop head; conduit wiring
  cross-checked against `xnio_conduits.rs` (out of scope) for the
  `set_channel_dispatcher` gap.
