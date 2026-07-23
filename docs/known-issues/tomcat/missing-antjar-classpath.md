# Missing `org.apache.tools.ant` on the Linux test classpath — 2 classes

> ✅ **RESOLVED, 2026-07-23.** Appended `/usr/share/java/ant.jar` +
> `/usr/share/java/ant-launcher.jar` (already installed via the host's `ant`
> apt package — no download needed) to `.suite/cp-linux-fixed.txt`.
> `TestJspC` now passes on both VMs. **`TestDeployTask` now reveals a real,
> separate CratonVM regression** (unrelated to the classpath fix): a `%20`
> in a file path isn't decoded back to a space when resolving a `file:` URL
> to read a jar, so `dir%20with%20spaces/context.jar` 404s even though
> `dir with spaces/context.jar` exists on disk. See
> [regressions-revealed-by-fixture-completion-20260723.md](regressions-revealed-by-fixture-completion-20260723.md).

**Not a CratonVM bug** (the classpath gap itself). Fails identically on real
JDK 25 (HotSpot) in the same fixture.

## Symptom

```
java.lang.NoClassDefFoundError: org/apache/tools/ant/Task
```

## Affected classes

- `org.apache.catalina.ant.TestDeployTask`
- `org.apache.jasper.TestJspC`

## Root cause

Both classes drive Ant's own `Task` API directly: `TestDeployTask` exercises
Tomcat's `DeployTask` Ant task, and `org.apache.jasper.JspC` (the JSP-to-
servlet precompiler under test in `TestJspC`) itself extends
`org.apache.tools.ant.Task`. The Linux harness's flat classpath file
(`.suite/cp-linux-fixed.txt`) has no `ant.jar`/`ant-launcher.jar` on it.

## Fix

Add Ant's own jars to the classpath. They're already staged on the Windows
harness at `apps\tomcat-suite-runner\.suite\apache-ant\lib\{ant.jar,ant-launcher.jar}`
(or install fresh: `sudo apt-get install ant` on the Azure host, then point
at `/usr/share/java/ant.jar` + `ant-launcher.jar`). Append both to
`cp-linux-fixed.txt` (or the `$CP` construction in
`apps/tomcat-suite-runner/run-tomcat-suite.sh`) and rerun both classes.

## Verify

```sh
CRATONVM_EXE=<binary> TC_ROOT=/data/data/apps/tomcat \
  bash apps/tomcat-suite-runner/run-tomcat-suite.sh craton 0 1 antjar-fix-verify \
  <(printf 'org.apache.catalina.ant.TestDeployTask\norg.apache.jasper.TestJspC\n')
```
Expect `PASS` on real JDK first (HotSpot control), then compare against
CratonVM.
