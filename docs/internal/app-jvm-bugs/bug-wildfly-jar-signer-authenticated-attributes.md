# WildFly — JAR signer rejects `authenticatedAttributes`-less SignerInfo

## Status
**OPEN (noise)** — boot continues; repeated warnings during module scan.

## Severity
**LOW** — performance / verification policy; not primary crash.

## App / suite
- **Context:** WildFly / JBoss Modules JAR scanning during boot
- **Logs:** wildfly-daemon and modload runs

## Symptom

```
jar signer: rejecting signer block: SignerInfo is missing authenticatedAttributes
```

Repeated for many JARs in `modules/` tree.

## HotSpot behavior

JDK accepts or processes signer blocks per standard JAR verification rules; boot proceeds without this rejection spam (or uses a different code path).

## CratonVM behavior

CratonVM JAR signature verification **rejects** SignerInfo blocks missing **authenticatedAttributes** — stricter or incorrect vs JDK 25.

## Root cause (suspected)

Custom `jar signer` / JAR verification native in CratonVM does not match JDK leniency for legacy signed JARs in WildFly distribution.

## Impact

- Slower boot (failed verification retries?)
- Possible skip of signed-JAR fast paths
- Unlikely root cause of functional failures once modules load

## Reproduce

Boot WildFly with module scan logging:

```bash
grep -i "jar signer" test-infra/suite-results/apps-four-*/wildfly-daemon.log
```

## What to fix

1. Compare CratonVM signer validation against `jdk.jar` / HotSpot for sample WildFly module JAR.
2. Align SignerInfo parsing with JDK 25 (authenticatedAttributes optional cases).
3. Confirm warning count drops; measure boot time.

## Related

- `apps/wildfly/CRATONVM_BUGS.md` Bug 5
