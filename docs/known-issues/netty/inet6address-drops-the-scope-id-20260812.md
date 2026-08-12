# `Inet6Address` drops the scope id — `getHostAddress()`/`getHostName()` never append `%scope`

**Status:** OPEN (2026-08-12). Behind
[netty investigate-batch-04](investigate-batch-04.md)'s
`io.netty.channel.unix.NativeInetAddressTest`. Azure Linux host
(`20.80.105.49`), binary built from `origin/dev` `8763197f2`.

## Symptom

```
NativeInetAddressTest.testLinkOnlyAddressIncludeScopeId
  expected: <fe80:3030:3030:3030:3030:3030:3030:3031%0>
   but was: <fe80:3030:3030:3030:3030:3030:3030:3031>
```

Reproduced with no netty on the classpath:

```java
byte[] linkLocal = { (byte)0xfe, (byte)0x80, '0','0','0','0','0','0','0','0','0','0','0','0','0','1' };
Inet6Address a = Inet6Address.getByAddress(null, linkLocal, 0);
Inet6Address b = Inet6Address.getByAddress(null, linkLocal, 7);
```

| | HotSpot JDK 25 | CratonVM |
| --- | --- | --- |
| `a.isLinkLocalAddress()` | `true` | `true` ✅ |
| `a.getScopeId()` | `0` | `0` ✅ |
| `a.getHostAddress()` | `fe80:…:3031%0` | **`fe80:…:3031`** |
| `a.getHostName()` | `fe80:…:3031%0` | **`fe80:…:3031`** |
| **`b.getHostAddress()`** (scope 7) | **`fe80:…:3031%7`** | **`fe80:…:3031`** |
| `a.toString()` | `fe80:…:3031%0/fe80:…:3031%0` | `/fe80:…:3031` |

The `%7` row is the one that makes this a defect rather than a formatting
nicety: a **non-zero** scope id is dropped too, so a scoped link-local address
round-trips to a different address. `getScopeId()` itself is right, so the
information exists and is lost on the way out.

`toString()` also loses the host half (`/addr` rather than `host/addr`) even
though `getHostName()` returns a non-empty literal on the same object, so those
two answers disagree with each other on CratonVM.

## Where it goes

`getHostAddress()` is registered on `java/net/InetAddress` (which intercepts
`Inet6Address` too) in `native-builtins/src/net_phase_e.rs` and is simply

```rust
r.register(ia, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
    let this = obj_arg(args, 0)?;
    Ok(Some(inet_addr_field(ctx, this, IA_ADDR)))
});
```

— a plain read of one synthetic text field, with no scope handling anywhere.
The real `Inet6Address.getHostAddress()` appends `'%' + scope_ifname` (or the
numeric `scope_id`) whenever the address carries a scope.

So the fix is not only in the formatter: the scope has to survive
**construction**. `net_phase_e.rs` already has the shape of the problem
recorded a few hundred lines away — it writes `scope_id` and `scope_id_set` as
`0` into the `Inet6AddressHolder` with the comment "not recoverable from a bare
`Ipv6Addr`, so leave scope_id unset (0)". Whatever path
`Inet6Address.getByAddress(host, addr, scopeId)` takes needs to keep the
caller's scope id, and the accessors need to read it back.

Note `native-io/src/net.rs` already does the scoped formatting correctly for
its own purposes (`read_inet_address_text` builds `format!("{v6}%{s}")` from
`holder6.scope_id`), so there is a working reference in-tree — it is just not
what the public accessors use.

## Impact

Anything that formats or parses a scoped IPv6 address: link-local peers on a
multi-interface host, `NetworkInterface`-derived addresses, and any config or
log line that round-trips `getHostAddress()`. Silent — the address just loses
its interface and then names a different destination.

## Repro

```bash
javac -d . NetProbe.java && cratonvm --java-home <jdk25> -cp . NetProbe   # section A
cd apps/netty-suite-runner
printf 'io.netty.channel.unix.NativeInetAddressTest\n' > /tmp/one.txt
bash run-netty-suite.sh --list /tmp/one.txt --shards 1 --timeout 300 --bin <cratonvm> --out /tmp/repro
```
