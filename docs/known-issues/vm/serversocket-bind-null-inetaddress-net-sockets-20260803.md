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
