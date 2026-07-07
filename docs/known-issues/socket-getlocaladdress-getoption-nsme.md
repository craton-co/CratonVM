# `java.net.Socket.getLocalAddress()` mis-dispatches to `java/lang/String.getOption` — NoSuchMethodError

Status: open (untriaged)

Date observed: 2026-07-07 (Azure Linux, branch
`fix/netty-sslengine-underflow-20260707` off dev; real-JDK jdk25)

## Context

Found while closing out
[`../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md`](../internal/reactive-netty-https-sslengine-handshake-underflow-FIXED.md):
once the SSLEngine handshake and the three stacked client-trust/session bugs
behind it were fixed, `ServerHttpsRequestIntegrationTests::checkUri()`
advanced past the TLS layer entirely and hit this new, unrelated failure.

## Symptom

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/String.getOption(I)Ljava/lang/Object;"
  caller="java/net/Socket.getLocalAddress()Ljava/net/InetAddress; @pc=22"

FAILCAUSE ServerHttpsRequestIntegrationTests :: checkUri() ::
  java.lang.NoSuchMethodError: java/lang/String.getOption(I)Ljava/lang/Object;
```

Real (interpreted) `java.net.Socket.getLocalAddress()` bytecode — not a
CratonVM native shim — makes an interface/virtual call that the VM resolves
against `java/lang/String` instead of the actual receiver (almost certainly
the socket's `SocketImpl`/`SocketOptions`-shaped delegate: JDK's
`Socket.getLocalAddress()` reads the bound address via
`getImpl().getOption(SocketOptions.SO_BINDADDR)`-style delegation). A
`getOption(I)Ljava/lang/Object;` signature resolving onto `String` strongly
suggests the interpreter's receiver-class bookkeeping for this call site is
wrong — either a stale/corrupt class-name lookup, or (given the receiver here
is one of CratonVM's synthetic native-tls-backed socket objects, see
`phases_late.rs`'s `new13_*` socket family) a synthetic-object class tag
that doesn't match what the interpreter's call-site resolution expects.

Not yet determined whether this is specific to the NEW-13 native-tls client
socket (synthetic `javax/net/ssl/SSLSocket`, see
`phases_late.rs::new13_do_create_socket`) being passed somewhere that expects
a plain `java/net/Socket`/real `SocketImpl`, or a more general interpreter
dispatch bug that any `Socket.getLocalAddress()` call would hit.

## Repro

```bash
CP=$(cat /data/data/spring-framework-shared/spring-web/build/cratonvm-testcp.txt)
RUNNER=/data/data/spring-suite-runner-shared
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  <cratonvm> --java-home /home/victor/jdk25 -cp "$RUNNER:$CP" \
  KRun org.springframework.http.server.reactive.ServerHttpsRequestIntegrationTests
```

## Next step

Confirm whether the receiver at the failing call site is a NEW-13 synthetic
socket object (check `new13_*` socket's class-name tag vs what
`Socket.getLocalAddress()`'s bytecode expects to invoke on) or a real
`java.net.Socket`/`SocketImpl`. If synthetic: either register a real
`getOption` native on the synthetic class, or (more likely correct) make
`Socket.getLocalAddress()` route through CratonVM's own `getLocalAddress`
native override instead of falling into real bytecode that assumes a real
`SocketImpl`. If not synthetic: this points at a general interpreter
call-site/receiver-class resolution bug worth its own isolated repro outside
the TLS suite.
