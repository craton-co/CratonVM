# `nsme_probe` — `NoSuchMethodError` message-shape oracle

HotSpot's `NoSuchMethodError` message is not the raw descriptor. It is
`'<return> <class>.<name>(<params>)'` — source spelling throughout, params
`, `-separated, **and the single quotes are part of the message**. That is easy
to get wrong and impossible to guess, so this probe measures it: compile
`v1/Lib.java` (which has the methods), compile the caller against it, then run
the caller against `v2/Lib.java` (which does not).

Written while triaging `module/spring-boot-data-redis` on the Azure Linux
fixture, where a `spring-data-redis` SNAPSHOT drifted ahead of the pinned
`jedis-7.4.1.jar` and made *both* VMs raise this error. The failure was shared,
so the natural triage is "not our bug" — but the messages differed, and that
difference was a real CratonVM defect. See
`data-redis-fixture-jedis-snapshot-skew-20260813.md` under
`known-issues/springboot/`.

## Run

```bash
JH=/path/to/jdk-25/bin
$JH/javac -d v1 v1/Lib.java
$JH/javac -cp v1 -d run run/NsmeProbe.java
$JH/javac -d v2 v2/Lib.java
$JH/java  -cp "run:v2" NsmeProbe     # HotSpot oracle
cratonvm --cp "run:v2" NsmeProbe     # must match byte for byte
```

## JDK 25 output (the pinned oracle)

```text
NSME_MESSAGE=['Lib Lib.widen(boolean)']
NSME_MESSAGE=['long Lib.calc(int, java.lang.String[], double[][])']
NSME_MESSAGE=['void Lib.plain()']
```

The three cases are load-bearing: a reference return type, an array/primitive
parameter mix (arrays use `int[]` source spelling, **not** the `[I` descriptor
form `ClassCastException` uses), and a `void` return with an empty parameter
list. The same assertions are unit-pinned in
`vm/src/runtime/exceptions.rs::helpful_npe_tests::nsme_message_matches_hotspot`.
