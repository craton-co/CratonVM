# `KeyStore.getInstance("PKCS12")` named a class nothing registered

## Status
**FIXED 2026-08-18**, same day it landed. Branch
`fix/jdksslenginetest-regression-20260818`.

## Measured

netty's `io.netty.handler.ssl.JdkSslEngineTest`, 821 tests, Azure host, one
class per process, per-method budgets lifted
(`-Djunit.jupiter.execution.timeout.mode=disabled`):

| build | ok | failed | aborted |
|---|---|---|---|
| `6fe3123f0` (before) | 754 | 1 | 66 |
| `24a5d4528` … `36433bf5d` (after) | **563** | **192** | 66 |
| the same, with `a44f3456f` reverted | 754 | 1 | 66 |
| **the same, + three registrations** | **754** | **1** | 66 |

The one remaining failure is `testSSLSessionId`, which fails on every arm
including the good ones — a pre-existing flake, not a residual.

## What happened

`a44f3456f` corrected the SUN provider's `KeyStore` service rows to the class
names a real JDK publishes — measured against jdk-25, and right:

```
SUN     KeyStore.JKS    -> sun.security.provider.JavaKeyStore$DualFormatJKS
SUN     KeyStore.PKCS12 -> sun.security.pkcs12.PKCS12KeyStore$DualFormatPKCS12
SUN     KeyStore.DKS    -> sun.security.provider.DomainKeyStore$DKS
```

`keystore.rs` registers its `engine*` surface **by class name, with no
inheritance walk** — its own comment says exactly that:

> we register on each FQN explicitly because dispatch is keyed by class name
> (no Java-inheritance walk on the native side)

So the moment a row started naming `…$DualFormatPKCS12`, the default PKCS#12
keystore resolved to a class with **no registration at all**: `engineLoad` and
friends became methods with no body. The JKS half of the very same change WAS
registered (`JavaKeyStore$DualFormatJKS` is in the list); only its twin was
missed.

netty builds all of its key material through `KeyStore`, so the blast radius
was the whole class. The tell in the wreckage:

```
java.io.IOException: setNeedClientAuth(true) requires javax.net.ssl.trustStore
```

— which is `t27_tls::default_engine_server_config`, i.e. an engine that fell
back to the no-`SSLContext` path because its context never got an identity.
Reading that message as a trust-store problem would have sent the next reader a
long way in the wrong direction; it is a *symptom* of an empty keystore three
layers down.

## The fix

Register the engine surface on every class the provider table advertises.
Three had none: `PKCS12KeyStore$DualFormatPKCS12` (the regression),
`JavaKeyStore$CaseExactJKS` and `DomainKeyStore$DKS` (both advertised earlier
and never registered, so not regressions — the same hole one door along).

`a44f3456f` is otherwise kept in full. Its five interop fixes are correct and
independently measured; only the registration list had failed to follow.

## The guard

`every_advertised_keystore_class_has_an_engine_surface` builds the registry and
asserts every advertised `KeyStore` class resolves to a registration.
Verified by removing the three registrations again: it fails, and the
companion test (`the_advertised_list_matches_the_provider_table`, which
compares the guard's list against `provider_chain.rs`'s source so the guard
cannot check a fiction) still passes — the right one fails, alone.

**Why nothing caught this.** The provider table was right. The registration
list was right for what it listed. The gap was *between* two files joined only
by a string, and no build step looked across it. That is the general shape:
when dispatch is keyed by a name, the table that publishes the name and the
table that serves it need a test that reads both.

## Repro
```bash
cd /data/cratonvm/apps/netty-suite-runner
printf 'io.netty.handler.ssl.JdkSslEngineTest\n' > /tmp/j.txt
CRATONVM_JAVA_EXTRA_ARGS="-Djunit.jupiter.execution.timeout.mode=disabled" \
CV_BIN=<binary> OUTROOT=/tmp/out SHARDS=1 TIMEOUT=2400 \
  bash run-netty-suite.sh --list /tmp/j.txt
```
This class cannot be measured on the Windows box — it exceeds 900 s there where
HotSpot takes 180 s. Azure only, and record the load average beside the result.
Read `aborted=` from `@@RESULT`: the runner prints `@@TESTFAIL` for aborted
tests too, and this class aborts 66 by assumption on every healthy run.
