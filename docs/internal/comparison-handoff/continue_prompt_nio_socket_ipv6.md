# NIO real-socket path: IPv6 addresses resolve to wildcard (Inet6Address.holder not read)

**Severity:** medium (IPv6 bind/connect on the real `sun/nio/ch/Net` path silently uses the wrong/wildcard address). Self-contained. Baseline commit: `d6cefc7` on `dev`. Only relevant under `CRATONVM_REAL_NET_SOCKETS=1`.

## Background
The real blocking-socket path (`java.net.Socket`/`ServerSocket` → `NioSocketImpl` → `sun/nio/ch/Net`) is now wired (commit `d6cefc7`). `sun.nio.ch.Net.bind0`/`connect0` receive a real-JDK `InetAddress` object; `native-io/src/net.rs::read_inet_address_text(ctx, ia)` extracts the dotted/colon address text.

The current implementation only handles **IPv4**: it reads `Inet4Address.holder.address` (an `int`, host byte order — 127.0.0.1 == 0x7F000001) via `get_field_by_name(o, "holder")` → `get_field_by_name(holder, "address")`, then formats `a>>24 . a>>16 . a>>8 . a` masked. For an `Inet6Address` the `holder.address` int is ~0, so v6 falls through to the text-field fallback (synthetic layout) and typically ends up `"0.0.0.0"` → wrong bind/connect target.

Real `java.net.Inet6Address` stores its 16-byte address in a SEPARATE holder: `Inet6Address.holder6` (an `Inet6Address$Inet6AddressHolder`) with a `byte[16] ipaddress` field (plus `scope_id`). Confirm exact field names via `"/c/Program Files/Java/jdk-25/bin/javap.exe" -p java.net.Inet6Address` and `java.net.Inet6Address$Inet6AddressHolder`, and extract the source from `C:/Program Files/Java/jdk-25/lib/src.zip` (`java.base/java/net/Inet6Address.java`).

## Fix
In `read_inet_address_text` (`native-io/src/net.rs` ~line 305), BEFORE the IPv4 holder read:
1. Detect an Inet6Address: e.g. `ctx.get_field_by_name(o, "holder6")` returns a non-null object, OR the object's class is `java/net/Inet6Address`.
2. Read the 16-byte `ipaddress` array (via `get_field_by_name(holder6, "ipaddress")` → array object → read bytes) and format as a canonical IPv6 string (`std::net::Ipv6Addr::from([u8;16]).to_string()`). Handle the `scope_id` if present (append `%scope`), or ignore for loopback.
3. Keep the existing IPv4 `holder.address` path and the synthetic text fallback for everything else.

Also audit the REVERSE direction:
- `net_local_inet_address` / `net_remote_inet_address` build an `InetAddress` from a Rust `IpAddr` text — confirm they produce a usable object for v6 (they currently allocate a 2-field synthetic InetAddress; a v6 `IpAddr` text round-trips through that fine, but verify real bytecode reading it).
- `net_accept`'s `isaa[0]` construction (`new InetSocketAddress(String, int)`) — for a v6 peer the host string is a v6 literal; confirm `InetSocketAddress(String,int)` resolves it.

## Verification
- IPv6 loopback round-trip with the gate ON: bind `new ServerSocket()` to `new InetSocketAddress(InetAddress.getByName("::1"), 0)`, connect a `new Socket("::1", port)`, write+read a byte, assert PASS. Add a `C:/tmp/audit/SockProbe6.java` modeled on `SockProbe2.java`.
- Trace with `CRATONVM_DBG_NET=1`: `[NET] connect0 ... addr=::1:NNNN` (not `0.0.0.0`).
- IPv4 still PASSES: `CRATONVM_REAL_NET_SOCKETS=1 cratonvm.exe -cp C:/tmp/audit SockProbe` → PASS.
- Regression pool stays 13/14.

## Key files
`native-io/src/net.rs` (`read_inet_address_text` ~305, `net_local_inet_address`, `net_remote_inet_address`, `net_bind0`, `net_connect0`, `net_accept`). JDK source: `java.net.Inet6Address` / `Inet6Address$Inet6AddressHolder`. Memory: `reference_server_socket_gap`.

## Build/test gotchas (Windows)
`taskkill //F //IM cratonvm.exe cargo.exe rustc.exe` + `rm -f target/release/cratonvm.exe` before each rebuild; verify exe mtime; ONE build at a time. See `reference_windows_exe_lock_build_trap`.
