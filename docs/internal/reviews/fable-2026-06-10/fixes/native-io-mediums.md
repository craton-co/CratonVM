# Fix note — native-io-mediums

Agent id: `native-io-mediums`
Report: `docs/reviews/fable-2026-06-10/native-io.md`
Scope (owned files only): `native-io/src/outbound_policy.rs`, `native-io/src/lib.rs`
Findings addressed: **B4** (low), **V2** (low, SSRF hardening), **V3** (low, doc/contract).

No `cargo`/`git` run (per pool rules). Edits are surgical and preserve all feature
configs (no new `cfg` gates introduced; the only new env knob is read at runtime, not
compile time).

---

## B4 — `host_part` mis-splits a bare (unbracketed) IPv6 literal

File: `native-io/src/outbound_policy.rs` (`host_part`, was line 158).

### Bug
`host_part("fe80::1")` (no brackets, no port) fell straight to `rsplit_once(':')`,
which split on the **last colon inside the address** and returned `"fe80:"`. That is
not a parseable `IpAddr`, so `default_policy` saw `host.parse::<IpAddr>()` fail and
returned `Allow` — letting an unbracketed link-local / cloud-metadata IPv6 literal
(`fe80::1`, `fd00:ec2::254`, …) bypass the default metadata block **at the
literal-string layer** when `check_outbound` is called directly with an unbracketed v6
host. (The two production connect paths — `policy_connect` here and
`socket_channel.rs::resolve_and_vet` — both re-bracket resolved v6 addresses as
`[{}]:{}` before re-vetting, so the end-to-end flow was already safe; the helper itself
was wrong for the bare-literal shape.)

### Fix
`host_part` now distinguishes the three real input shapes:
- bracketed IPv6 (`[::1]:80` → `::1`, `[fe80::1]` → `fe80::1`) — handled first, unchanged;
- `host:port` with **exactly one** colon (`127.0.0.1:80`, `example.com:443`) — strip the port;
- a **bare IPv6 literal** with **two or more** colons and no brackets — return the
  string **whole** so it parses as an `IpAddr`.

Implementation: after the bracket branch, `if target.matches(':').count() >= 2 { return target; }`
before the `rsplit_once(':')` fallback. Single-colon and no-colon inputs are unaffected,
so the only behavioural change is for the previously-mangled `>=2`-colon unbracketed case
— i.e. exactly the bug.

### Tests added
- `host_part_handles_bare_ipv6_literal` — bare `fe80::1` / `::1` / `fd00:ec2::254` and
  bracketed-no-port `[fe80::1]` all round-trip to a parseable `IpAddr`.
- `default_policy_denies_bare_ipv6_metadata` — end-to-end: unbracketed `fe80::1`,
  `fd00:ec2::254`, and bracketed-with-port `[fd00:ec2::254]:80` are all `Deny`; a public
  v6 literal `2001:4860:4860::8888` stays `Allow` (no false positive).

---

## V2 — default outbound policy: opt-in loopback + RFC1918 blocking

File: `native-io/src/outbound_policy.rs` (`default_policy` + new classifier helpers).

### Gap
The default policy only denies link-local cloud-metadata (`169.254.0.0/16`,
`fe80::/10`, `fd00:ec2::254`). SSRF to `127.0.0.1`, `10/8`, `172.16/12`, `192.168/16`,
`::1` was allowed by default. This is the documented metadata-only default; the task is
to let a confined workload opt in to a stricter posture **without changing the default**.

### Fix (gated, default-off)
Added an opt-in env flag **`CRATONVM_BLOCK_PRIVATE_NETS`** (presence = on;
`0`/`false`/`off`/`no` = off, case-insensitive — same parse the crate's other
`CRATONVM_*` flags use). When engaged, `default_policy` *additionally* denies:
- IPv4 loopback `127.0.0.0/8` and RFC1918 `10/8`, `172.16/12`, `192.168/16`;
- IPv6 loopback `::1` and unique-local `fc00::/7` (the v6 RFC1918 equivalent);
- IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is classified by the embedded v4 octets so a
  mapped private/loopback address can't tunnel past.

The always-on link-local metadata block runs **first and unconditionally**, so the
opt-in flag only ever *adds* denials. The flag is read once and cached in a tri-state
`AtomicU64` (`0`=uncomputed, `1`=off, `2`=on) so the hot connect path stays a cheap
branch and the "absent" default is never confused with "uncomputed". **Default-off
behaviour is byte-for-byte unchanged** — with the flag unset, only link-local metadata
is blocked, exactly as before. Matches the report's Feature-Suggestion #4.

### Tests added
- `private_net_classifier_covers_loopback_and_rfc1918` — pure-classifier matrix:
  loopback/RFC1918/ULA/IPv4-mapped-private are denied; public v4/v6, the `172.15`/`172.32`
  edges just outside the /12, `192.169`, and a public IPv4-mapped v6 are allowed.
- `private_net_classifier_excludes_link_local_metadata` — `169.254.169.254` is handled
  by the metadata classifier, not the private classifier (no mis-bucketing).

Tests exercise the pure classifier (`is_private_or_loopback_ip`) directly rather than
toggling the process-global env cache, so they don't race other tests.

---

## V3 — `validate_path` absolute-path behaviour: doc/contract tightening

File: `native-io/src/lib.rs` (`validate_path`, the confinement-off early return ~line 342).

### Status
Per the report this is **by-design, not a code defect**. The behaviour is already
correct and already tested:
- confinement **OFF** (default) accepts absolute paths with no `..` segment
  (JDK-faithful for single-tenant `java -jar`) — `path_validation_accepts_absolute_path_by_default`;
- confinement **ON** rejects any absolute path / symlink that canonicalizes outside the
  sandbox root via `is_within_sandbox` —
  `path_validation_rejects_out_of_sandbox_absolute_when_confined`.

The task was to "add a clear doc comment and ensure that when confinement IS on, absolute
paths escaping root are rejected." The rejection already holds; I verified the
canonicalize-then-`is_within_sandbox` gate (lib.rs ~444) covers the absolute-escape case
and is regression-tested.

### Change
Added a focused inline comment at the confinement-off early return naming V3 explicitly:
it documents that THIS line is the "absolute paths accepted when confinement is OFF"
contract, that it is by-design for single-tenant launches, how to turn confinement on
(`set_path_confine_to_cwd(true)` / `CRATONVM_CONFINE_IO` / `CRATONVM_UNTRUSTED_CODE`), and
that the confinement-ON path falls through to the `is_within_sandbox` containment check
that rejects out-of-sandbox absolute/symlink resolutions (cross-referencing the existing
regression test). No behavioural change.

---

## Compile / config safety
- No new types or APIs; `host_part` keeps its `&str -> &str` signature.
- `Ipv6Addr::to_ipv4_mapped` (1.63), `is_loopback`, `segments`, `Ipv4Addr::octets` are all
  well under the workspace MSRV 1.77.
- New code is plain runtime logic (no `cfg`-gated branches), so default, `app-stubs`, and
  `synthetic-jdk` configs all compile identically.
- `host_part` is a private fn with no callers outside this file; `socket_channel.rs` /
  `datagram.rs` only *mention* it in comments and already bracket v6 before vetting, so no
  caller contract changed.

## Not in my scope (other agents' files / slices)
B1/B2/B3 (bulk/abs bounds), B5 (process dir slot), V1 (subprocess confinement), S1
(`t16_dc_*` DatagramChannel stub), P1–P4 — these live in `process.rs`, `nio_native.rs`,
`direct_buffer.rs`, `net.rs`, `zip_real_jar.rs`, or the bulk/abs natives, and are covered
by the sibling notes (`native-io-buffers.md`, `native-io-datagram.md`,
`native-io-subprocess.md`). Untouched here.
