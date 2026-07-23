# Missing `output/build/lib/*.jar` — 1 class

> ✅ **RESOLVED, 2026-07-23** (and turned out to matter far more broadly than
> just `TestTomcatNoServer` — see below). `cd /data/data/apps/tomcat &&
> JAVA_HOME=/home/victor/jdk25 ant deploy` (5 seconds, `BUILD SUCCESSFUL`)
> populated `output/build/lib/` with all 32 expected jars
> (`tomcat-util.jar`, `catalina.jar`, `tomcat-coyote.jar`, etc.).
> `TestTomcatNoServer` now passes on both VMs. **This same fix was actually
> the real blocker for most of the 8 classes documented in
> [missing-catalina-localhost-context-configs.md](missing-catalina-localhost-context-configs.md)**
> — that doc's original root-cause guess (a missing `conf/Catalina/localhost/`
> directory) was wrong; the missing `lib/` jars were the actual cause of
> most of those `LifecycleException`s. See that doc's correction note and
> [regressions-revealed-by-fixture-completion-20260723.md](regressions-revealed-by-fixture-completion-20260723.md)
> for the 6 classes that turned out to be real CratonVM regressions once
> this was fixed.

**Not a CratonVM bug** (the missing-jars gap itself).

## Symptom

```
java.io.FileNotFoundException: /data/data/apps/tomcat/output/build/lib/tomcat-util.jar
```

## Affected classes

- `org.apache.catalina.startup.TestTomcatNoServer`

## Root cause

`output/build/lib/` doesn't exist in this fixture at all. Ant's `deploy`
target's `package`/`build-tomcat-jdbc` dependencies populate it with
`tomcat-util.jar` and a handful of other jars; this fixture was only ever
taken through `ant test-compile` (to get compiled test classes + a
classpath), never the packaging steps.

## Fix

Run the real Ant targets that populate `output/build/lib/`:

```sh
cd /data/data/apps/tomcat
ant package   # or: ant deploy (broader, also touches conf/ and webapps/)
```

If a full `ant package` run is impractical on this host, at minimum copy the
built `tomcat-util.jar` (and whatever else `TestTomcatNoServer` needs — check
its imports / classpath usage first) from a from-scratch build into
`output/build/lib/`.

## Verify

Rerun `org.apache.catalina.startup.TestTomcatNoServer` under HotSpot; expect
PASS once the jar is present. Worth a broader sweep after this fix — other
classes not caught in this rerun may also depend on `output/build/lib/`
jars that happen not to have been exercised by the 195-class subset checked
here.
