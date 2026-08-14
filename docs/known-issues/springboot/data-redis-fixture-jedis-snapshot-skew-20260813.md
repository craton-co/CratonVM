# `module/spring-boot-data-redis` is red on the Azure Linux fixture — `spring-data-redis` SNAPSHOT drifted ahead of the pinned Jedis — NOT a CratonVM defect

**Both VMs fail identically.** HotSpot and CratonVM produce the same statuses,
the same test counts and the same failing test names on this fixture. Nothing
here is a VM bug, and nothing here is a regression of the `URLClassLoader`
classpath-rescan cluster (that page is retired to `fixed-suite-bugs/springboot/`
as `data-redis-urlclassloader-uncached-classpath-hang-FIXED.md`). This page
exists so the next person who sees these three rows red does **not** reopen
that one.

## Symptom

On the Azure Linux host (`20.80.105.49`), against the shared fixture at
`/data/cratonvm/apps/spring-boot`:

| Class | HotSpot | CratonVM |
|---|---|---|
| `DataRedisAutoConfigurationJedisTests` | 23 tests, **21 failed** | 23 tests, **21 failed** |
| `DataRedisHealthContributorAutoConfigurationTests` | 2 tests, **2 failed** | 2 tests, **2 failed** |
| `DataRedisAutoConfigurationTests` | 56 tests, **1 failed** (`connectionFactoryWithJedisClientType`) | 56 tests, **1 failed** (same test) |

Every failure is the same `NoSuchMethodError`, raised while Spring builds the
`redisConnectionFactory` bean:

```text
java.lang.NoSuchMethodError: 'redis.clients.jedis.DefaultJedisClientConfig$Builder
  redis.clients.jedis.DefaultJedisClientConfig$Builder.autoNegotiateProtocol(boolean)'
    at org.springframework.data.redis.connection.jedis.JedisConnectionFactory.<init>
```

## Root cause: a floating SNAPSHOT met a pinned release

The caller is not fixture source — nothing in the checkout calls
`autoNegotiateProtocol`. It is compiled into the `spring-data-redis` jar, and
the two hosts resolved **different** ones:

| | `spring-data-redis` | `jedis` | `autoNegotiateProtocol` |
|---|---|---|---|
| Windows local | `4.1.0-RC1` (released) | `7.4.1` | not called — **green** |
| Azure Linux | `4.2.0-SNAPSHOT` | `7.4.1` | called — **red** |

`4.2.0-SNAPSHOT` is a moving target: it was fetched at some point after the
Gradle cache had already pinned `jedis` at `7.4.1`, and that snapshot is built
against a Jedis that has `DefaultJedisClientConfig$Builder.autoNegotiateProtocol(boolean)`.
`7.4.1` does not have it (`javap` on
`redis.clients.jedis.DefaultJedisClientConfig$Builder` lists `protocol(RedisProtocol)`
and no `autoNegotiate*`), and `7.4.1` is the only Jedis version in
`/data/toolchain/gradle-home/caches/modules-2/files-2.1/redis.clients/jedis/`.
So the classpath is internally inconsistent and every code path that constructs
a `JedisConnectionFactory` dies at link time, on any JVM.

## Fix (fixture, not VM)

Refresh the module's dependency resolution on the Azure host so the two move
together — re-run the suite runner's `-Setup`/`-RefreshClasspaths` for
`module/spring-boot-data-redis` with network access, which should pull the
Jedis the current `spring-data-redis` snapshot expects (or pin
`spring-data-redis` to a release consistent with `jedis 7.4.1`).

**Not done here on purpose:** `/data/cratonvm/apps/spring-boot` is the shared
fixture every session on that host runs against, and regenerating its
classpaths mid-flight would move other sessions' baselines underneath them.
Coordinate before doing it.

## How to tell this apart from a real VM bug in one command

Run the class under HotSpot on the *same* fixture. If the counts match
CratonVM's, it is this page:

```bash
cd /data/cratonvm/apps/spring-boot
CP="sb-runner:$(cat module/spring-boot-data-redis/build/cratonvm-test-cp.txt)"
/data/toolchain/jdk-25/bin/java -cp "$CP" SbRunner \
  org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests
```

## What was NOT this, and is genuinely fixed

- **The `DataRedisAutoConfigurationTests` HANG.** The 2026-08-12 run
  `regression96-verify-gen-20260812` recorded it as `HANG` at 1044s on binary
  `cratonvm-envfix-linux-20260812`. On current `dev` (`05fff8409`, binary
  `cratonvm-redisdoc-linux-20260813`) the same class on the same fixture
  completes in **64s** under the default collector and **73s** under
  `-XX:+UseGenerationalGC`, 56 tests, 1 failed — matching HotSpot exactly. The
  hang is gone; only the fixture failure above remains.
- **CratonVM's `NoSuchMethodError` message shape**, which was wrong and was
  found *inside* this shared failure — see below. Fixed 2026-08-13.

## The real CratonVM defect this fixture gap was hiding

Both VMs failed, but they did not say the same thing. CratonVM emitted the
dotted class name followed by the **raw descriptor**, with no return type and
no quotes:

```text
CratonVM (before)  redis.clients.jedis.DefaultJedisClientConfig$Builder.autoNegotiateProtocol(Z)Lredis/clients/jedis/DefaultJedisClientConfig$Builder;
HotSpot            'redis.clients.jedis.DefaultJedisClientConfig$Builder redis.clients.jedis.DefaultJedisClientConfig$Builder.autoNegotiateProtocol(boolean)'
```

Fixed in `vm/src/runtime/exceptions.rs` (`nsme_message`), pinned by unit tests
to the JDK 25 output of the new `apps/nsme_probe` oracle. This is the payoff
for diffing the *message* rather than stopping at "both VMs fail, so it is not
ours".
