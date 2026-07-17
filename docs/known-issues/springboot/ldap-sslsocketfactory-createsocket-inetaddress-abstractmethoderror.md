# Embedded LDAP with SSL bundle: `SocketFactory.createSocket(InetAddress,int)` resolves to the abstract declaration — `AbstractMethodError`

**Status: OPEN — found 2026-07-17**

## Symptom

`module/spring-boot-ldap`'s `EmbeddedLdapAutoConfigurationTests` fails 1 of
its 17 tests:

```
JUnit Jupiter:EmbeddedLdapAutoConfigurationTests:whenSslBundleIsConfiguredLdapsListenerIsConfigured()
    => java.lang.AbstractMethodError: method javax/net/SocketFactory.createSocket(Ljava/net/InetAddress;I)Ljava/net/Socket; has no Code attribute
```

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard4/logs/module_spring-boot-ldap.org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoC-3c6a8c4df0c6.out.log`

## Root cause (confirmed registration exists; dispatch-path gap not pinned)

`javax.net.SocketFactory.createSocket(InetAddress, int)` is `public
abstract` in the real JDK — every concrete `SocketFactory` subclass (in
particular whatever concrete `SSLSocketFactory` implementation the embedded
LDAP server's SSL bundle configuration obtains, e.g. via
`SSLContext.getSocketFactory()`) must supply its own override, or CratonVM
must supply one natively for the concrete class actually instantiated. An
`AbstractMethodError` here means bytecode dispatch on the receiver resolved
all the way up to `javax/net/SocketFactory`'s own class-file declaration of
this method (which, being abstract, correctly has no `Code` attribute) —
i.e. **no override was found anywhere between the concrete receiver class
and `SocketFactory`**.

This is confirmed **not** simply "unregistered anywhere": CratonVM *does*
register a Bridge-category native for exactly this `(class, name,
descriptor)` triple, on `javax/net/SocketFactory` itself:

```rust
// native-builtins/src/phases_early.rs:10851-10867, register_phase52_server_socket_factory
r.register(
    sf,   // "javax/net/SocketFactory"
    "createSocket",
    "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
    |ctx, args| { /* ... phase52_socket_connect(...) ... */ },
);
```

So a plain `javax.net.SocketFactory.getDefault()` instance (whose runtime
class is registered as literally `"javax/net/SocketFactory"`) would resolve
this call fine. The failure here is for a **different, concrete**
`SSLSocketFactory`-family receiver (the embedded LDAP server's TLS socket
factory) that does not carry this native and has no real-bytecode override
of its own either — its vtable/method-resolution walk apparently does not
fall through to the phases_early.rs Bridge registration on the ancestor
`javax/net/SocketFactory` class, and instead hits the receiver's own (or an
intermediate `javax/net/ssl/SSLSocketFactory`'s) unimplemented abstract
slot, which — being backed by the real JDK class file for that abstract
declaration — has no `Code` attribute, producing `AbstractMethodError`
rather than falling back to the ancestor's native.

**Likely related to, but distinct from, today's fix in
`docs/internal/fixed-suite-bugs/wrong-receiver-virtual-dispatch-corruption-cluster-FIXED.md`**
(fixed earlier the same day, 2026-07-17): that fix's Case 1 covered
`javax/net/ssl/SSLSocketFactory.createSocket(String,int)` and
`createSocket(Socket,String,int,boolean)` specifically (`native-builtins/src/tls.rs:1591,1608`)
by excluding legacy `SyntheticStub` registrations for those two overloads
under `CRATONVM_REAL_NET_SOCKETS=1` and relying on later "P68 Bridge"
registrations. **This `(InetAddress,int)` overload was not mentioned in
that fix** (only `(String,int)` and the 4-arg layered-socket overload are),
and grepping `native-builtins/src/tls.rs` and the other `javax/net/ssl/SSLSocketFactory`
registration sites (`net_phase_e.rs:8514,8698,8910`, `t27_tls.rs:2832,2875`,
`phases_late.rs:42520,42575,70567,70574` per that doc's own audit list)
turns up no `createSocket(InetAddress,int)` registration on the
`SSLSocketFactory` class name anywhere — only the ancestor
`javax/net/SocketFactory` registration cited above exists. This looks like
a narrower, adjacent gap in the same "TLS-aware `SocketFactory` overload
coverage" area rather than the same bug recurring: not an object-layout
corruption (Case 1's mechanism) but a missing/non-inherited method
implementation for one specific overload on the SSL-aware subclass.

**Not independently confirmed via live debugging this session** — no
rebuild or breakpoint/trace was done. The `phases_early.rs` registration
existing-but-apparently-unreached is established by source inspection only;
whether the actual defect is in how CratonVM's SSL socket factory class is
synthesized (missing its own override, when it should either implement
`createSocket(InetAddress,int)` itself or the method-resolution walk should
consult ancestor-class native registrations before declaring
`AbstractMethodError`) was not traced end-to-end.

## Affected classes

| Module | Class |
|---|---|
| `module/spring-boot-ldap` | `org.springframework.boot.ldap.autoconfigure.embedded.EmbeddedLdapAutoConfigurationTests` (1 of 17 tests) |
