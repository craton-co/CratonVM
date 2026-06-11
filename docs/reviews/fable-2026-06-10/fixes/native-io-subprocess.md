# Fix note — `native-io-subprocess`

Owner files: `native-io/src/process.rs`, `native-io/src/datagram.rs`
Source report: `docs/reviews/fable-2026-06-10/native-io.md` (V1 high, B5 low, S1 policy)

---

## V1 (HIGH) — Subprocess spawn is not sandbox-confined; bypasses CWD confinement

### Finding
`spawn_and_wrap` (process.rs) validated only the **working directory**
against `validate_path`. The **executable** (`program`) was passed straight to
`Command::new(program)`. Under the certified/untrusted profile
(`CRATONVM_CONFINE_IO` / `set_path_confine_to_cwd(true)`) guest file
reads/writes are confined to the CWD sandbox, but untrusted bytecode could
still `new ProcessBuilder("/bin/sh", ...).start()` or
`Runtime.exec("C:\\Windows\\System32\\cmd.exe ...")` and spawn an arbitrary
host program that inherits the JVM's full ambient authority — a far larger
escape than the file-path reads the confinement profile blocks. The module doc
claimed the certified profile "fails closed", but process spawn was wide open.

### Root cause
Missing executable-path gate: only `work_dir` went through `validate_path`; the
program argument had no confinement check at all.

### Exact change (process.rs)
1. New helper `validate_spawn_program(program: &str) -> Result<(), RuntimeError>`
   added just above `spawn_and_wrap`:
   - **Confinement OFF (default, JDK-faithful single-tenant):** returns `Ok` for
     any program — behaviour unchanged, matching `validate_path`'s default of
     letting `java -jar app.jar` reach the host freely.
   - **Confinement ON (fail closed):**
     - Rejects a **bare command name** (no `/` or `\`), because
       `Command::new("sh")` PATH-resolves to an arbitrary host binary unrelated
       to the sandbox — un-range-checkable, so denied.
     - For an explicit path, requires `validate_path(program)` to succeed (the
       fully-resolved path must stay inside the sandbox root), reusing the exact
       canonicalize-then-contain check used for every guest file access.
   - NUL bytes are rejected unconditionally (host C-string truncation), in both
     profiles.
2. `spawn_and_wrap` now calls `validate_spawn_program(program)` immediately
   after the empty-program check and **before** building the `Command`. A
   `SecurityException` from the gate is mapped to `IOException` (the same
   exception `ProcessBuilder.start` / `UNIXProcess.forkAndExec` raise for an
   unusable command), matching how the sibling work-dir validation already maps
   its rejection.

This covers all three spawn entry points (`ProcessImpl.create`,
`UNIXProcess.forkAndExec`, `ProcessBuilder.start`) because they all funnel
through `spawn_and_wrap`.

### Test added
`validate_spawn_program_confinement_gate` (process.rs `#[cfg(test)]`):
asserts confinement-off accepts `sh` / `/bin/sh` / `cmd.exe`; confinement-on
rejects bare `sh`/`cmd` and an explicit out-of-sandbox absolute path
(`/bin/sh` on unix, `C:\Windows\System32\cmd.exe` on windows); NUL always
rejected. Restores the global confine flag to `false` at the end so it does not
leak to other tests.

---

## B5 (LOW) — ProcessBuilder.start reads working dir from a fixed File slot 0

### Finding
`native_process_builder_start` read the directory as
`ctx.get_field(file_obj, 0)` (synthetic File layout). A real-JDK
`java.io.File` does not place its `String path` field at slot 0, so
`ProcessBuilder.directory(dir)` was silently ignored (spawned in CWD instead of
`dir`). The sibling command-list extraction directly above was already fixed to
read `size`/`elementData` by name; the directory read was not given the same
treatment.

### Exact change (process.rs)
Read the File's path **by name** first
(`ctx.get_field_by_name(file_obj, "path")`), falling back to slot 0 only for the
synthetic File layout. This mirrors the established `get_field_by_name(file,
"path")` convention used across the codebase (e.g. jboss_module_loader.rs:681,
native-builtins/lib.rs:10393).

---

## S1 (POLICY) — synthetic DatagramChannel fabricates state

### Finding (as routed to me)
Task said: gate the synthetic DatagramChannel in `datagram.rs` behind the
`synthetic-jdk` cfg feature so the default build does not fabricate datagram
state.

### Why no change was made to `datagram.rs` — and where the real fix belongs
The fabrication the report's S1 actually describes is **not in `datagram.rs`**.
Re-reading the report (native-io.md S1) and the code:

- The state-fabricating shim is `t16_dc_connect` (and the `t16_dc_*` family)
  in **`native-io/src/nio_native.rs:1133`** — it invents a `"127.0.0.1:9"`
  target when the `SocketAddress` can't be decoded, **ignores the result of the
  underlying `udp.connect()`** ("we still mark the channel as connected"), and
  unconditionally sets the connected flag (slot 2 = 1). That is the
  policy-violating "fake connected state" finding.
- **`datagram.rs` is the REAL implementation.** The report explicitly says "The
  real DatagramChannel implementation lives in `datagram.rs`; these overrides
  shadow it." Every native in `datagram.rs` is backed by a real
  `std::net::UdpSocket` (`bind`/`send_to`/`recv_from`/`join_multicast_*`/…),
  registered under `NativeKind::Bridge` (datagram.rs:618), and `alloc_channel`
  initialises `connected = 0`. It contains **no** `connect` handler and
  fabricates **no** false app-observable state. (`dgram_block`/`dgram_unblock`
  return the real `MembershipKey` and are a documented IGMPv3-source-specific
  limitation, not a fabricated success.)

Gating `datagram.rs` behind `synthetic-jdk` (off by default) would do the
**opposite** of the policy intent: it would remove the *real* datagram
transport from the default build (breaking the WP3.7 send/receive/multicast
acceptance path) while leaving the *fabricating* `t16_dc_connect` stub in
`nio_native.rs` untouched and still default-on. That would worsen the policy
violation.

**Per the ABSOLUTE RULES** ("If a correct fix needs a file you do not own, STOP
and document it precisely in the fix-note"), the S1 fix must be made in a file I
do **not** own.

### Precise out-of-scope fix needed (for the owner of `nio_native.rs`)
File: `native-io/src/nio_native.rs`
- `t16_dc_connect` (lines ~1133–1166): stop fabricating. Either
  (a) gate the whole `t16_dc_*` DatagramChannel family behind
  `#[cfg(feature = "synthetic-jdk")]` and tag it `NativeKind::SyntheticStub`, so
  the default build falls through to the real `datagram.rs` path; or
  (b) make it faithful — propagate the `udp.connect()` error instead of
  discarding it, and do not default a `"127.0.0.1:9"` target / do not set the
  connected flag on failure.
- Registration site: `register_t16_channel_overrides` (nio_native.rs:1222),
  invoked from `register_io_natives` in `lib.rs` (~4254), is registered LAST so
  it currently wins over `register_datagram_real`. If option (a) is taken, the
  real `datagram.rs` registrations should be the default winner for
  `DatagramChannel.connect`/`isConnected`.
- Note `nio_native.rs` carries `#[cfg(test)]` tests that assert the *fabricated*
  connected flag (report Tests section); those encode the policy-violating
  behaviour and must be updated alongside the fix.

I could not make this change because `nio_native.rs` is outside my owned-file
set, and `lib.rs` (the registration-ordering / feature-gate wiring) is also
outside it.

---

## Files touched
- `native-io/src/process.rs` — V1 spawn-program confinement gate + helper +
  test; B5 work-dir-by-name read.
- `native-io/src/datagram.rs` — **no change** (it is the real implementation;
  S1 fix belongs in `nio_native.rs`, see above).
- `docs/reviews/fable-2026-06-10/fixes/native-io-subprocess.md` — this note.

## Tests added
- `validate_spawn_program_confinement_gate` (process.rs).

## Follow-up & risk
- **V1 risk:** Low. Default (unconfined) profile is byte-for-byte unchanged —
  the gate is a no-op unless confinement is explicitly turned on. Under
  confinement, the policy is intentionally strict (bare names rejected,
  explicit paths must be in-sandbox); an embedder that needs to spawn a host
  tool from a confined profile must place it inside a sandbox root or register
  that root via `add_sandbox_root`. This matches the "fails closed" contract the
  module doc already advertises.
- **B5 risk:** Low. By-name read with slot-0 fallback; synthetic File layout
  still works, real-JDK File now honoured.
- **S1 follow-up:** OPEN — requires an edit to `native-io/src/nio_native.rs`
  (and possibly the registration order in `native-io/src/lib.rs`). Not done here
  (file not owned). Details above.
- Possible future hardening (not done, out of scope): a dedicated
  process-spawn policy hook (allow/deny by program path) mirroring
  `outbound_policy`, so embedders can install a custom allowlist rather than
  relying on sandbox-root containment alone.
