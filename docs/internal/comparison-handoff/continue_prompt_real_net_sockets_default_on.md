# Flip CRATONVM_REAL_NET_SOCKETS default-ON, delete the synthetic java.net socket surface, strip debug traces

**Severity:** medium (finishes the no-synthetic-stubs migration for `java.net.Socket`/`ServerSocket`; unblocks daemon/server apps without an env gate). Self-contained but **DEPENDS ON** the other NIO continue-prompts landing green first. Baseline commit: `d6cefc7` on `dev`.

## DEPENDENCY — do these FIRST
Do NOT flip the default until the following are merged + verified, or you risk regressing TLS/HTTP/daemon apps:
- `continue_prompt_arena_native_memory_routing.md` (R1 silent-corruption + R2 FileDispatcher SIGSEGV) — otherwise file-channel I/O on the real path can crash.
- `continue_prompt_varhandle_getandadd_meta.md` (real VarHandle ops).
- `continue_prompt_nio_socket_ipv6.md` (v6 bind/connect).
- `continue_prompt_nio_regression_coverage.md` (so default-mode regressions are actually caught).

## Background
`CRATONVM_REAL_NET_SOCKETS` (default-OFF) is a central registry filter in `native-api/src/registry.rs` (`real_net_sockets_enabled()` + the `register()` filter) that DROPS the synthetic `java/net/Socket`/`ServerSocket` natives so real bytecode → `sun/nio/ch/Net` runs. With it ON, the full bind/connect/accept/read/write/close round-trip PASSES (SockProbe/SockProbe2). The synthetic natives still exist (registered from ~6 functions) as the default. This task makes the real path the default and removes the synthetic surface.

NOTE (from review): the `java/net/ServerSocket` allow-list entry was already removed from `vm/src/vm/vm_exec.rs` but is INERT in default mode — the primary dispatch is `try_stackless_invoke` (`vm/src/runtime/interpreter.rs` ~11273) which calls `native_methods.find()` unconditionally. So the *registry filter* (dropping the registration) is the real lever; rely on it.

## Plan (in order)
1. **Wider soak with the gate ON** before changing any default:
   - Daemon bind+serve: embedded Tomcat (`tomcat-server-info` pool app + a real HTTP probe), Kafka broker, a `ServerSocketChannel` echo, blocking `ServerSocket` echo — confirm they bind a real port and accept.
   - Client paths: `HttpURLConnection`/`java.net.http.HttpClient` GET to a local server; `SSLSocket`/`SSLServerSocket` handshake (TLS uses `javax.net.ssl.SSLSocket` which extends `Socket` — verify dropping the `java.net.Socket` synthetic natives doesn't break the SSL socket surface; `t27_tls.rs` registers on `javax/net/ssl/SSLServerSocket`, a different class, so it's unaffected — but confirm the SSL impl's delegate Socket works on the real path).
   - The full regression pool stays green.
2. **Flip the default**: make `real_net_sockets_enabled()` default true (or invert to an opt-OUT `CRATONVM_LEGACY_NET_SOCKETS`). Keep an escape hatch env for one release.
3. **Delete the synthetic surface** once default-ON soaks clean: remove the `java/net/Socket`/`ServerSocket` registrations in `register_phase53_socket_stubs` (`native-builtins/src/phases_early.rs`), `register_p72_server_socket` (`phases_late.rs`), `register_re1_socket`/`register_re2_server_socket` (`net_phase_e.rs`). Keep `java/net/SocketInputStream`/`SocketOutputStream` only if still referenced (they aren't on the real path). Drop the now-dead `vm_exec.rs` allow-list comment and the redundant per-fn env gates.
4. **Strip debug traces** added during the NIO work: the `dbgnet!` macro + call sites in `native-io/src/net.rs`, the `dbgplain!` macro + sites in `native-builtins/src/plain_socket.rs`, and the `CRATONVM_DBG_NET` `eprintln!` in `native-builtins/src/shared_secrets_bridge.rs` (`jnio_new_direct_byte_buffer`). (These are gated and socket-path-only, but should not ship in the default path. NOTE: the hot-path `CRATONVM_DBG_TOARRAY` eprintlns elsewhere are unrelated pre-existing dev cruft — leave them to their owner.)

## Verification
- Default build (no env): `cratonvm.exe -cp C:/tmp/audit SockProbe` → PASS (real port + round-trip), proving the real path is now default.
- Tomcat/Kafka/WildFly daemon smoke bind a real port and serve.
- TLS `SSLSocket` round-trip works.
- Regression pool green (with the new coverage probes).
- `--dump-native-registry` shows NO `java/net/Socket`/`ServerSocket` natives (synthetic surface deleted).

## Key files
`native-api/src/registry.rs` (gate). `native-builtins/src/{phases_early,phases_late,net_phase_e,plain_socket,shared_secrets_bridge}.rs`. `native-io/src/net.rs`. `vm/src/vm/vm_exec.rs`, `vm/src/runtime/interpreter.rs`. Memory: `reference_server_socket_gap`, `project_synthetic_stub_removal`, `feedback_no_synthetic_stubs`.

## Build/test gotchas (Windows)
`taskkill //F //IM cratonvm.exe cargo.exe rustc.exe` + `rm -f target/release/cratonvm.exe` before each rebuild; verify exe mtime; ONE build at a time. See `reference_windows_exe_lock_build_trap`.
