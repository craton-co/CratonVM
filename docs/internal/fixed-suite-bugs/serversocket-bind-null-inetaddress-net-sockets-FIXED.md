# `ServerSocket.bind` passed a null `InetAddress` into `Net.bind` under `CRATONVM_REAL=net-sockets` — FIXED

| | |
|---|---|
| **Status** | ✅ **FIXED / RETIRED** 2026-08-04 (filed 2026-08-03) |
| **Area** | `native-builtins/src/{net_phase_e,phases_early,lang_class}.rs`, `native-builtins/src/lib.rs`, `native-io/src/{lib,socket_channel}.rs` |
| **Original symptom** | `new ServerSocket(0)` → `NullPointerException: Cannot invoke "java.net.InetAddress.isLinkLocalAddress()" because "addr" is null`, thrown inside `sun/nio/ch/Net.bind` |
| **Was failing** | `nio_selector_selected_keys_survives_gc_stress` (`vm/tests/nio_selector_build_set_gc.rs`), which had been `#[ignore]`d pointing here |
| **Retired by** | branch `fix/serversocket-null-inetaddress-20260804` |

The original write-up is preserved verbatim at the bottom.

---

## What it actually was

Not a socket bug at all. **Two independent cross-call GC-safety defects**, both
of which only bite on a COLD VM, and both of which the doc's reproducer
(`CRATONVM_GC=stress=65536`) made deterministic.

The doc's "Next step" — *"start at `Net.bind`'s two overloads and find where the
wildcard `InetAddress` is dropped … most likely an `InetSocketAddress` whose
address field is never populated when the port is 0, or a native `bind0`
signature mismatch"* — was wrong on both counts. The address field IS populated;
the object it is populated into is the one the collector just vacated.

### The observable that made it findable

`probes/ServerSocketNullInetAddressProbe.java` asserts a **paired** property on
every `InetSocketAddress` it builds:

```java
i.isUnresolved() == (i.getAddress() == null)   // the JDK's own definition
```

That pair is what makes the failure reach `Net.bind` at all. `ServerSocket.bind`
guards itself:

```java
if (epoint.isUnresolved())
    throw new SocketException("Unresolved address");
...
impl.bind(epoint.getAddress(), epoint.getPort());
```

A merely-null address would have been rejected there with a clear
`SocketException`. What actually happened is that `getAddress()` answered
**null** while `isUnresolved()` answered **false** — the guard passed, and the
null travelled two frames further to `Net.bind`, whose very first act is
`addr.isLinkLocalAddress()`.

### Defect 1 — the `InetAddress` / `InetSocketAddress` construction path

`ServerSocket(int port, int backlog, InetAddress bindAddr)` builds
`new InetSocketAddress(bindAddr, port)` with a **null** `bindAddr`, and the JDK
substitutes the wildcard. CratonVM implements that constructor natively
(`phases_early.rs`, `register_phase52_inet_socket_address`), and the chain
`<init>` → `alloc_inet_address_external` → `alloc_inet_address` →
`populate_inet_holder` → `p52_isa_set` held **every** freshly-allocated object
as a bare `ObjectRef` local across calls that allocate.

On a cold VM the first `alloc_concurrent_synthetic` in that chain also **loads
and initialises** `java/net/InetAddress`, `java/net/Inet4Address` and
`java/net/InetAddress$InetAddressHolder` — a lot of Java, and therefore a
reliable moving young collection. `moving_young` is DEFAULT-ON
(`types/src/flags.rs`: opt out with `CRATONVM_NO_MOVING_YOUNG`), so
`CRATONVM_GC=stress=65536` alone reproduces; the `moving-young` token the
original test also set is redundant.

After that collection the address mirror had relocated, and:

* `populate_inet_holder` wrote `holder` into the vacated from-space copy, and
* `alloc_inet_address` returned that stale reference to its caller, which
  stored it in the `InetSocketAddress` holder's `addr` slot.

Reading `addr` back through the relocated holder yields null. `isUnresolved()`
did not agree because it reaches the same slot by a different route: with the
holder's `addr` gone, `p52_isa_addr_value` falls through to `get_field(this, 2)`
— an out-of-range read on a real-layout `InetSocketAddress`, which is not
`Value::Object(None)` and so reads as "resolved".

**Cold-only, and the ORDER is load-bearing.** Resolving any `InetAddress`
first warms the hierarchy and hides the defect completely. That is what pinned
it down: on the pre-fix binary,

| first statement | `new InetSocketAddress(0).getAddress()` |
|---|---|
| *(nothing)* | **null** |
| `InetSocketAddress.createUnresolved("x", 1)` — loads the ISA holder class only | **null** |
| `InetAddress.getByAddress(new byte[] {1,2,3,4})` — warms the InetAddress hierarchy | `0.0.0.0` |

i.e. the window is inside the InetAddress allocation, not the
`InetSocketAddress` one.

### Defect 2 — `Class.getEnumConstants()` copying out of a relocated `$VALUES`

Fixing defect 1 was not enough: `new ServerSocket(0)` then died with
`ExceptionInInitializerError` ← `IllegalArgumentException: No enum constant
WINDOWS`, from `jdk/internal/util/OperatingSystem.<clinit>` reached through
`sun/nio/ch/Net.<clinit>` → `ExtendedSocketOptions`.

`native_class_get_enum_constants` (`lang_class.rs`) did:

```rust
let len = ctx.array_length(src_arr);
let out = ctx.new_ref_array(class_id, len);   // ALLOCATES — can move src_arr
for i in 0..len {
    let v = ctx.get_array_element(src_arr, i); // stale
    ctx.set_array_element(out, i, v);
}
```

so after a relocation it returned a correctly-**sized** array of **nulls**.
`Enum.valueOf` scans that array, found nothing, and threw for a constant that
plainly exists. `Enum.valueOf` itself had the same defect one level up: it held
`constants` and each `candidate` across `invoke_virtual(candidate, "name")`,
which runs Java.

Minimal repro, no sockets involved (pre-fix, `CRATONVM_GC=stress=65536`):

```java
enum E { LINUX, MACOS, WINDOWS, AIX;
    static final E CUR = E.valueOf("windows".toUpperCase(Locale.ROOT)); }
// ExceptionInInitializerError: No enum constant WINDOWS
```

`Locale.ROOT` is not incidental — loading `java.util.Locale` inside the enum's
`<clinit>` is what supplies the collection between `$VALUES` being assigned and
`valueOf` reading it. With a literal `"WINDOWS"` (no class load in between) the
same enum initialises fine, which is why this had never been seen.

## What changed

All of it is the same correction: root through a `NativeHandleScope` and re-read
before every use, per `docs/feature-designs/native-handle-discipline.md`.

* `net_phase_e::populate_inet_holder` — roots the mirror and both holders, and
  now **returns** the mirror's current address instead of leaving the caller
  holding its own stale copy.
* `net_phase_e::alloc_inet_address` — propagates that returned reference.
* `net_phase_e::alloc_inet_socket_address` and
  `alloc_inet_socket_address_resolved` — same treatment.
* `phases_early::p52_isa_set` and all three `InetSocketAddress` constructors
  plus `createUnresolved` — same treatment.
* `lang_class::native_class_get_enum_constants` — roots source and destination
  arrays around `new_ref_array`.
* `java/lang/Enum.valueOf` (`native-builtins/src/lib.rs`) — roots the constants
  array and each candidate across the `name()` dispatch.
* `native-io::socket_channel::new_resolved_inet_socket_address` — roots the host
  String and the `InetAddress` across the two re-entrant `invoke`s. Same window
  `net_accept` already pinned for; it was unrooted here.

Two API-fidelity defects the probe surfaced in the same code are fixed with it:

* **`InetSocketAddress` accepted an out-of-range port.** The JDK runs `checkPort`
  in every constructor and throws `IllegalArgumentException("port out of
  range:N")`; ours stored the value. `new InetSocketAddress(-1)` produced a
  socket address with a negative port that only failed much later, as an
  unrelated OS error. Now `p52_isa_check_port`, applied to all three
  constructors and `createUnresolved`.
* **`DatagramChannel.getLocalAddress()` returned the same broken pair.**
  `native_dc_local_addr` hand-wrote the legacy two-slot layout — a bare host
  String in slot 0, where a real-layout `InetSocketAddress` keeps its `holder` —
  so `getAddress()` was null while `isUnresolved()` was false, on a channel that
  was demonstrably bound. It now builds through the real
  `(Ljava/lang/String;I)V` constructor.
* **`InetSocketAddress.toString()`** emitted a bare `host:port`, so an
  unresolved address printed exactly like a resolved one. It now renders
  HotSpot's `hostName/ip:port` and `hostname/<unresolved>:port`.

## Verification

`probes/ServerSocketNullInetAddressProbe.java` — 5 sections, 50+ normalised
lines over `InetAddress`, `InetSocketAddress`, `ServerSocket`, and the four
channel types, recorded on **HotSpot 25.0.3+9 first** and then diffed. Values
that vary per run are reduced to properties (`portPositive`, `pairOk`) so a
correct VM prints byte-identical output.

`vm/tests/wildcard_bind_under_gc_stress.rs` +
`vm/tests/resources/cratonvm/WildcardBindUnderGcStress.java` pin the contract in
three arms (`stress=65536`, `stress=65536,moving-young`, and no stress at all —
the last so a future "fix" that only works when collections are frequent is
caught). **Mutation-checked against the pre-fix binary**, which fails all three:

| arm | pre-fix | post-fix |
|---|---|---|
| `stress=65536` | `AssertionError: new InetSocketAddress(0).getAddress() is null on a cold VM` | PASS |
| `stress=65536,moving-young` | same | PASS |
| no stress | `AssertionError: new InetSocketAddress(-1) did not throw` | PASS |

`vm/tests/nio_selector_build_set_gc.rs::nio_selector_selected_keys_survives_gc_stress`
is **un-ignored**, with its assertions unchanged.

The fixture's check order is deliberate and documented in the fixture: nothing
above the first `new InetSocketAddress(0)` may touch `java.net`, or the defect is
warmed away and the test passes vacuously.

## Not fixed — recorded so it is not mistaken for this bug

* **`ServerSocketChannel.getLocalAddress()` reports loopback for a wildcard
  bind** (`127.0.0.1` where HotSpot reports `0:0:0:0:0:0:0:0`). This is
  deliberate: `socket_channel::advertised_listener_host` substitutes loopback
  because a wildcard listener address is a valid bind target but **not** a valid
  client connect destination on Windows (`WSAEADDRNOTAVAIL` / os error 10049),
  and Netty / Jetty's `ServerConnector` / `sun.net.httpserver.ServerImpl`
  publish this address for clients to reconnect to. Changing it would regress
  those. The channel binds the wildcard correctly; only the *advertised* address
  differs.
* **Channels report an IPv4 wildcard where HotSpot reports the IPv6 one.**
  HotSpot's dual-stack channels answer `Inet6Address 0:0:0:0:0:0:0:0`;
  CratonVM's answer `Inet4Address 0.0.0.0`. Pre-existing and unrelated.
* **`InetAddress.toString()` keeps a hostName for literal-derived addresses.**
  `InetAddress.getByName("127.0.0.1").toString()` is `127.0.0.1/127.0.0.1` here
  and `/127.0.0.1` on HotSpot — HotSpot leaves `hostName` null when the input
  was numeric. Every CratonVM mirror is built by `alloc_inet_address(host, ip)`
  with both fields set, so this is a wide, separate change; it is NOT a residual
  of this bug. It is the only remaining difference in the probe's
  `InetSocketAddress.toString()` lines, and it is filed on its own as
  `docs/known-issues/vm/inetaddress-tostring-keeps-a-hostname-for-literal-addresses-20260804.md`
  rather than left implicit here.

After the fix, the probe's whole diff against HotSpot under
`CRATONVM_GC=stress=65536` is **10 lines, all three bullets above and nothing
else** — the entire `ServerSocket` section (`s3.*`) is byte-identical, and
`pairOk=true` on every address in every section. Before the fix the same arm
produced `s1.wildcardViaIsa=null`, `s3.ctor0=THREW ExceptionInInitializerError`,
and four `NoClassDefFoundError`s behind it.

---

## Original write-up (2026-08-03), verbatim

# `ServerSocket.bind` passes a null `InetAddress` into `Net.bind` under `CRATONVM_REAL=net-sockets`

**Status: OPEN, newly revealed, not fixed. 2026-08-03.**
**Fails:** `nio_selector_selected_keys_survives_gc_stress`
(`vm/tests/nio_selector_build_set_gc.rs`).

## What happens

```
Exception in thread "main" java/lang/NullPointerException:
  Cannot invoke "java.net.InetAddress.isLinkLocalAddress()" because "addr" is null
	at cratonvm/NioSelectorBuildSetGc.main(NioSelectorBuildSetGc.java:28)
	at java/net/ServerSocket.<init>(ServerSocket.java:171)
	at java/net/ServerSocket.<init>(ServerSocket.java:278)
	at java/net/ServerSocket.bind(ServerSocket.java:391)
	at sun/nio/ch/NioSocketImpl.bind(NioSocketImpl.java:636)
	at sun/nio/ch/Net.bind(Net.java:566)
	at sun/nio/ch/Net.bind(Net.java:574)
```

`new ServerSocket(0)` on the real JDK's bytecode reaches `Net.bind`, and the
`InetAddress` that should carry the wildcard address arrives `null`.

## Why it only surfaced now

**This test has not been measuring anything since the flag rename.** It set four
per-flag environment variables:

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1  CRATONVM_REAL_NET_SOCKETS=1
CRATONVM_GC_STRESS=65536             CRATONVM_MOVING_YOUNG=1
```

Per-flag `CRATONVM_*` variables are now **rejected at startup**: the launcher
prints the supported grouped spelling and refuses to boot. So the VM exited
before running a line of Java, the test saw a non-zero status, and the failure
looked like "the selector broke under GC stress" when nothing had started.

Translated to the grouped spelling —

```
CRATONVM_THREADS=-default-watchdog
CRATONVM_REAL=net-sockets
CRATONVM_GC=stress=65536,moving-young
```

— the VM boots, runs, and hits the NPE above. Note the two GC tokens share one
`CRATONVM_GC`: a second assignment replaces the first rather than adding to it.

So the defect is **pre-existing and was masked**, not introduced by the flag
translation. The translation is what made the test able to fail honestly.

## Next step

Start at `Net.bind`'s two overloads in the real-JDK path and find where the
wildcard `InetAddress` is dropped — most likely an `InetSocketAddress` whose
address field is never populated when the port is 0, or a native `bind0`
signature mismatch that leaves the argument slot empty. `CRATONVM_REAL=net-sockets`
is required to reproduce; without it the socket layer is synthetic and never
reaches this path.

## What was done here

The flag spelling is fixed in the test — that part is a real repair and should
stay. The test itself is `#[ignore]`d with a reason pointing here, so the
now-honest failure is tracked rather than either hidden or left permanently red.
Un-ignore it as the fix lands; the assertion is unchanged.
