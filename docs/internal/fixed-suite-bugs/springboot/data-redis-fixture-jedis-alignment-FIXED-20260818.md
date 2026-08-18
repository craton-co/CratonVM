# `module/spring-boot-data-redis` is green on both VMs — the Jedis pin was a stale *alignment*, and the refresh task could never refresh

**Status: FIXED 2026-08-18** on the Azure Linux fixture host (`20.80.105.49`),
`/data/cratonvm/apps/spring-boot`. Retires
`known-issues/springboot/data-redis-fixture-jedis-snapshot-skew-20260813.md`.

All three classes now pass on **both** VMs, where all three were red on both:

| Class | before (both VMs) | after (both VMs) |
|---|---|---|
| `DataRedisAutoConfigurationJedisTests` | 23 tests, **21 failed** | 23 tests, **0 failed** |
| `…autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests` | 2 tests, **2 failed** | 2 tests, **0 failed** |
| `DataRedisAutoConfigurationTests` | 56 tests, **1 failed** | 56 tests, **0 failed** |

CratonVM was built from `dev` `c3c593f01` on the host; HotSpot is
`/data/toolchain/jdk-25`. The two agree exactly, which is the same thing the
open page said before the fix and is worth keeping: this row never
distinguished the VMs.

## The open page's diagnosis was right but not specific enough, and the imprecision hid the real defect

It said: *"a floating SNAPSHOT met a pinned release"* — `spring-data-redis
4.2.0-SNAPSHOT` moved, `jedis` was pinned at `7.4.1`, so the classpath went
internally inconsistent. True as far as it goes, and it points at "refresh the
resolution" as the fix.

What it misses is that **Spring Boot never intended to pin Jedis
independently**. `platform/spring-boot-dependencies/build.gradle` declares:

```groovy
library("Jedis", "7.4.1") {
    alignWith {
        property {
            name "jedis"
            of "org.springframework.data:spring-data-redis"
            managedBy "Spring Data Bom"
        }
    }
}
```

The `7.4.1` literal is a **cached copy of an alignment** that is supposed to
track `spring-data-redis`'s own `jedis` property. That snapshot's POM now says
`<jedis>8.0.0</jedis>`. So the literal was stale *by the fixture's own declared
rule* — not an arbitrary pin that happened to collide with a snapshot. Naming it
that way matters, because it says exactly which number is authoritative
(`spring-data-redis`'s) and which one follows.

## The defect the open page's fix instruction would have hit

The open page prescribes re-running the suite runner's
`-Setup`/`-RefreshClasspaths`. **That would have appeared to work and changed
nothing.** `cratonvm-test-cp.init.gradle` registers `cratonvmTestCp` with an
output and **no declared inputs**:

```groovy
def outFile = new File(proj.buildDir, 'cratonvm-test-cp.txt')
task.outputs.file(outFile)
task.doLast { outFile.text = entries.join(File.pathSeparator) }
```

Gradle therefore calls it `UP-TO-DATE` forever: once the file exists it is never
rewritten, however the resolved classpath changes. Observed directly here — a
successful re-resolve that pulled `jedis 8.0.0` into the Gradle cache left the
classpath file untouched, mtime still **Aug 12**:

```
> Task :module:spring-boot-data-redis:cratonvmTestCp UP-TO-DATE
```

That is why the skew persisted for five days instead of self-healing on the next
setup run, and it is the part a reader of the old page could not have known.
Fixed with `task.outputs.upToDateWhen { false }` — the task writes one text
file, so re-running it always is far cheaper than a stale snapshot that silently
breaks every run that reads it.

## What was changed, all of it on the fixture host

1. `platform/spring-boot-dependencies/build.gradle` — `library("Jedis", "7.4.1")`
   → `"8.0.0"`, restoring the alignment to what `spring-data-redis
   4.2.0-SNAPSHOT` declares. Verified before editing that `jedis-8.0.0` really
   carries the missing method:
   `javap` on `redis.clients.jedis.DefaultJedisClientConfig$Builder` lists
   `public …$Builder autoNegotiateProtocol(boolean)`.
2. `cratonvm-test-cp.init.gradle` — the `upToDateWhen { false }` above.
3. `module/spring-boot-data-redis/build/cratonvm-test-cp.txt` — regenerated; now
   carries `jedis-8.0.0.jar` beside `spring-data-redis-4.2.0-SNAPSHOT.jar`.

Backups of both edited files are at `/tmp/spring-boot-dependencies.build.gradle.bak`
and `/tmp/cratonvm-test-cp.init.gradle.bak`, and the pre-fix classpath at
`/tmp/cratonvm-test-cp.redis.bak.txt`. The two source edits are one line each and
show in `git diff` in that checkout, so reverting is trivial.

## Consequence to know about before the next full suite run

`upToDateWhen { false }` means the **next full-suite `-Setup` run regenerates
every module's classpath**, not just this one. That is the correct behaviour —
it is what makes a refresh actually refresh — but it is also precisely the
"moves other sessions' baselines underneath them" effect the open page warned
about, now armed for every module rather than avoided. Anyone comparing against
a pre-2026-08-18 baseline on this host should re-baseline rather than assume
drift is a regression.

Only `module/spring-boot-data-redis` was regenerated in this pass; every other
module's `cratonvm-test-cp.txt` is still the file it had, untouched.

## The `NoSuchMethodError` message fix still stands

The open page's genuinely-CratonVM finding — the `NoSuchMethodError` message
shape being the raw descriptor rather than HotSpot's
`'<return> <class>.<method>(<args>)'` — was fixed 2026-08-13 in
`vm/src/runtime/exceptions.rs` (`nsme_message`). Nothing here changes it. Note
that this fixture no longer produces that exception at all, so the unit tests
pinned to the `apps/nsme_probe` oracle are now the only thing holding that
behaviour: **do not** rely on this suite to catch a regression in it.

## Verifying

```bash
cd /data/cratonvm/apps/spring-boot
CP="sb-runner:$(cat module/spring-boot-data-redis/build/cratonvm-test-cp.txt)"
for c in \
  org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationJedisTests \
  org.springframework.boot.data.redis.autoconfigure.health.DataRedisHealthContributorAutoConfigurationTests \
  org.springframework.boot.data.redis.autoconfigure.DataRedisAutoConfigurationTests ; do
  /data/toolchain/jdk-25/bin/java -cp "$CP" SbRunner "$c" | grep SBRUNNER_RESULT
  <cratonvm> --java-home /data/toolchain/jdk-25 -cp "$CP" SbRunner "$c" | grep SBRUNNER_RESULT
done
```

Note the **corrected class name** in the middle row: the health-contributor test
moved into an `…autoconfigure.health.…` package, so the open page's name now
raises `ClassNotFoundException` and the runner reports `tests=0 … LOADFAIL`,
which reads like a pass to anything counting failures. Another instance of the
same trap as the `UP-TO-DATE` one — a harness that finds nothing looks exactly
like a harness that finds nothing wrong.

## Related

- `data-redis-urlclassloader-uncached-classpath-hang-FIXED.md` — the earlier,
  genuinely-VM defect on this same module. Still fixed; unrelated to this.
- `known-issues/springboot/data-redis-fixture-jedis-snapshot-skew-20260813.md` —
  retired by this page.
