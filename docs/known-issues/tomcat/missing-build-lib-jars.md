# Missing `output/build/lib/*.jar` — 1 class

**Not a CratonVM bug.** Fails identically on real JDK 25 (HotSpot) in the
same fixture.

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
