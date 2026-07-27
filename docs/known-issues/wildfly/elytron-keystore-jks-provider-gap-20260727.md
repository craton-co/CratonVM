# WildFly boot: `applicationKS` fails with `no KeyStore JKS implementation for provider` — `Security.getProviders(<filter>)` returns null and `Provider$Service` has no `aliases`

**Status:** OPEN, 2026-07-27. Not a regression and not JIT-related — reproduces
with `--nojit` and with every JIT ban configuration. Found while closing
`docs/internal/fixed-suite-bugs/wildfly/modeltypevalidator-validtypes-npe.md`;
this is the only thing standing between a real WildFly 32 boot and a clean
`WFLYSRV0025`.

## Symptom

Every `standalone.sh` boot of WildFly 32.0.1.Final now reaches
`WFLYSRV0026: … started (with errors) … Started 273 of 522 services (5 services
failed or missing dependencies …)` instead of `WFLYSRV0025`. All five failures
are one dependency chain rooted at:

```
TRACE [org.wildfly.security.tls] AtomicLoadKeyStore creating:  type = JKS,  provider =  sun.security.provider.JavaKeyStore$JKS
WARN  [org.wildfly.extension.elytron] WFLYELY00023: KeyStore file '…/configuration/application.keystore' does not exist. Used blank.
ERROR [org.jboss.msc.service.fail] MSC000001: Failed to start service org.wildfly.security.key-store.applicationKS
    org.jboss.msc.service.StartException: WFLYELY00004: Unable to start the service.
  Caused by: java.io.IOException: ELY02009: Unable to create a new KeyStore instance
  Caused by: java.security.KeyStoreException: JKS not found
  Caused by: java.security.NoSuchAlgorithmException: no KeyStore JKS implementation for provider
```

Note the trailing empty provider name in the last line — `Security.getImpl`
renders `provider.getName()` there, so the `Provider` object elytron resolved
has an empty/absent name. Note also the TRACE line printing the *SPI class*
(`sun.security.provider.JavaKeyStore$JKS`) where HotSpot prints
`SUN version 25`: elytron logs the `Provider`, so its `toString()` is wrong too.

The keystore file genuinely does not exist in the distribution — WildFly
generates it on first boot — so the `WFLYELY00023 … Used blank` warning is
expected on HotSpot as well; the boot only fails because the blank
`KeyStore.getInstance("JKS", provider)` behind it cannot be created.

## Isolated reproduction (no WildFly)

```java
import java.security.*;

public class KsProbe {
    public static void main(String[] args) throws Exception {
        System.out.println("getInstance(JKS) = " + KeyStore.getInstance("JKS"));
        Provider[] ps = Security.getProviders("KeyStore.JKS");
        System.out.println("providers for KeyStore.JKS = " + (ps == null ? "null" : ps.length));
        KeyStore ks = KeyStore.getInstance("JKS");
        ks.load(null, "changeit".toCharArray());
        System.out.println("blank load OK, size=" + ks.size());
    }
}
```

| | HotSpot (JDK 25) | CratonVM (`--nojit`, `CRATONVM_JAVA_HOME=jdk25`) |
|---|---|---|
| `KeyStore.getInstance("JKS")` | ok | ok |
| `Security.getProviders("KeyStore.JKS")` | **1 provider (`SUN`)** | **null** |
| blank `load(null, pw)` | ok, size 0 | ok, size 0 |

A second probe over `Security.getProviders()` shows the shape of the gap:

| | HotSpot | CratonVM |
|---|---|---|
| provider count | 12 | 13 |
| `p.getName()` for the JKS provider | `SUN` | (see below) |
| `p.getService("KeyStore","JKS")` | `SUN: KeyStore.JKS -> …DualFormatJKS` | — |
| `p.getProperty("KeyStore.JKS")` | `sun.security.provider.JavaKeyStore$DualFormatJKS` | — |

On CratonVM the enumeration itself throws before printing any row:

```
java.lang.NullPointerException: Cannot invoke "java.util.List.isEmpty()"
  because "this.aliases" is null
    at java/security/Provider$Service.toString(Provider.java:2176)
```

i.e. the `Provider$Service` objects CratonVM hands back are constructed without
their `aliases` list.

## What is broken

Three separable gaps, in the order a caller hits them:

1. **`Security.getProviders(String filter)` ignores `"<type>.<algorithm>"`
   filters** and returns `null` instead of the matching providers.
   `MessageDigest.SHA-256` behaves the same way, so this is the filter
   implementation, not something JKS-specific.
2. **`Provider$Service.aliases` is null** on the service objects reachable from
   a `Provider`, so any caller that touches them (including
   `Provider$Service.toString`) NPEs.
3. **`KeyStore.getInstance(String type, Provider provider)`** cannot find the
   JKS implementation for a provider that `KeyStore.getInstance(String)` is
   perfectly able to serve — the two paths disagree.

WildFly's elytron subsystem uses (1) and (3): `AtomicLoadKeyStore.newInstance`
takes a resolved `Provider` and calls `KeyStore.getInstance(type, provider)`
inside its `engineLoad`, so the working no-provider overload is never reached.

## No HotSpot control for the full boot

WildFly 32.0.1.Final cannot boot on HotSpot JDK 25 on this host at all:

```
Exception in thread "main" java.lang.UnsupportedOperationException:
  Setting a system-wide Policy object is not supported
    at java.base/java.security.Policy.setPolicy(Policy.java:114)
    at org.jboss.modules.Main.main(Main.java:391)
```

(`Policy.setPolicy` was removed in JDK 24+; CratonVM tolerates it.) The
comparison above therefore comes from the standalone probes, which run on both
VMs, rather than from a side-by-side boot.

## Reproduction of the boot

See the "Reproduction" section of
`docs/internal/fixed-suite-bugs/wildfly/modeltypevalidator-validtypes-npe.md` —
identical harness, no JIT flags needed.
