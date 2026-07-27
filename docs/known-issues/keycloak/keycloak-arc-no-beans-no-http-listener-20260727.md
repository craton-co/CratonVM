# OPEN — Keycloak boots but Arc registers no beans, so no HTTP listener starts

**Status: OPEN, found 2026-07-27** as a follow-on to the (now closed) Keycloak
boot blockers (classloader synthetic-stub fabrication pre-empting a custom
`ClassLoader`, root-caused and fixed). Not a
regression — this surface was simply unreachable before, because the boot died
earlier.

## Symptom

`kc.sh start-dev` on the real `keycloak-quarkus-dist-26.6.1` completes startup
under CratonVM: the main thread reaches
`io.quarkus.runtime.ApplicationLifecycleManager.waitForExit →
AbstractQueuedSynchronizer$ConditionObject.awaitUninterruptibly`, the normal
"started, waiting for shutdown" state. But:

- the last ~40 startup log lines are all
  `DEBUG [io.quarkus.arc.runtime.BeanContainerImpl] No matching bean found for
  type class <X> and qualifiers []. The bean might have been marked as unused
  and removed during build.` — for essentially every RESTEasy Reactive
  serialiser, body handler and exception mapper, ending with
  `io.quarkus.rest.runtime.__QuarkusInit`;
- the process has only **3 registered threads** (`main`, `Common-Cleaner`,
  `Timer-0`) — no Vert.x event-loop or executor threads;
- consequently no HTTP listener opens and the
  `Keycloak … started in Ns / Listening on http://…` banner never prints.

Reproduce (≈3-5 min):

```bash
JAVA_HOME=<dir whose bin/java is the CratonVM binary> \
CRATONVM_JAVA_HOME=/home/victor/jdk25 \
  bash bin/kc.sh start-dev --http-enabled=true --hostname-strict=false
```

Thread state is confirmable without gdb via
`CRATONVM_DEFAULT_WATCHDOG_SEC=420` (T19.H1 watchdog: thread summary + per
-thread Java stacks, then abort). `kill -QUIT` does NOT produce a Java thread
dump — it kills the process.

## What is known

`ArcContainerImpl.<init>` DOES construct successfully — the earlier
`ClassCastException: … _Synthetic_Bean cannot be cast to
io.quarkus.arc.InjectableBean` (a synthetic-stub mis-resolution) is fixed, and
Arc's generated `Default_ComponentsProvider_addBeans0` /
`ValueRegistry_…_Synthetic_Bean` classes now load from
`lib/quarkus/generated-bytecode.jar` under the real `RunnerClassLoader`. So the
beans are *loadable*; something after that leaves the container's bean map
empty or key-mismatched.

`BeanContainerImpl.instance(Class, Annotation...)` looks beans up by `Class`
identity, which is the shape CratonVM has repeatedly got wrong before (a
`Class`-keyed map missing an entry its name-keyed sibling finds) — see the
Spring analogues in memory
`spring-bean-class-identity-check-native-shims-first`. That is the first thing
to check, not the last: whether the `Class` object the generated
`ComponentsProvider` registered under and the one `BeanContainerImpl` looks up
with are the same `ClassId`.

## Why it is not blocking the JIT work

The two JIT bans this fixture existed to re-test (KC26-PIC.1, KC26-RX.1) were
measured and lifted against this same boot: Picocli CLI parsing, SmallRye
config mapping, Quarkus augmentation, JCA/BouncyCastle registration, Hibernate
ORM + Liquibase and Arc init all execute. Only the HTTP-serving phase is
missing.
