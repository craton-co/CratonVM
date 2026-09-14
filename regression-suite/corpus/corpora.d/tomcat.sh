# Apache Tomcat 12. Verified built on this host 2026-08-12.
#
# THE ANT BUILD IS NOT NEEDED. The campaign brief describes the Tomcat runner
# as wanting a ~20-minute `ant` build; on this host the build output is
# ALREADY PRESENT and complete:
#
#   output/classes        2798 .class files   (the server itself)
#   output/testclasses    1881 .class files   (the test suite)
#   output/build/lib        35 jars           (catalina.jar, jasper.jar, ecj-4.39.jar, ...)
#
# so the classpath composes straight off disk. Only /data/cratonvm/apps/tomcat has
# this; there is no tomcat under /data/cratonvm/apps (only a
# tomcat-suite-runner, which is a runner, not a corpus -- the presence of a
# `*-suite-runner` directory says nothing about whether the corpus is built).

CORPUS_DESC="Apache Tomcat 12 server classes + its own test suite"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="ant build NOT required; output/ is already populated on this host"

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/tomcat"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="/data/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"

# Left empty on purpose: naming one here would assert an expectation about
# which Tomcat test is supposed to pass, which this lane has not established.
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -f "$r/output/classes/org/apache/catalina/startup/Tomcat.class" ] || return 1
  [ -d "$r/output/testclasses" ] || return 1
  [ -n "$(ls -A "$r/output/testclasses" 2>/dev/null)" ] || return 1
  return 0
}

# Add the jars of the HIGHEST version of one maven artifact.
#
# `cp_add_jars_r "$m2/org/junit/platform"` sweeps the whole group recursively,
# so it puts EVERY cached version of every artifact on the classpath at once.
# The local repository holds junit-platform 1.12.1 and 1.14.4, and both landed.
# JUnit detects that itself and refuses to run:
#
#   JUnitException: Some JUnit versions on the classpath are not upward compatible
#     - org.junit.jupiter.engine:      5.14.4
#     - org.junit.platform.commons:    1.12.1
#   Caused by: java.lang.NoSuchMethodError:
#     'org.junit.platform.engine.OutputDirectoryCreator
#      org.junit.platform.engine.EngineDiscoveryRequest.getOutputDirectoryCreator()'
#
# It dies in DISCOVERY, before any test runs, on BOTH arms -- so every class
# scored a vacuous AGREE at ~2 s with rc=1 on each side. An agreeing failure is
# not an oracle, and a 12/12 built out of them says nothing about the VM.
cp_add_newest_artifact() {
  local ad="$1" v best=""
  [ -d "$ad" ] || return 0
  while IFS= read -r v; do [ -d "$ad/$v" ] && best="$v"; done < <(ls -1 "$ad" 2>/dev/null | sort -V)
  [ -n "$best" ] || return 0
  cp_add_jars "$ad/$best"
}

corpus_classpath() {
  local r="$1"
  cp_add "$r/output/classes"
  cp_add "$r/output/testclasses"
  cp_add_jars "$r/output/build/lib"
  # Tomcat's own third-party test dependencies (junit, easymock, ...) live
  # alongside the build, not in output/build/lib.
  cp_add_jars_r "$r/output/build/webapps/examples/WEB-INF/lib"
  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"

  # Tomcat 12's tests are JUnit *4* sources -- `org.junit.Test`,
  # `org.junit.Assume`, `@RunWith(Parameterized.class)` -- executed by the
  # junit-vintage engine. Without junit 4 itself the vintage engine discovers
  # nothing, so pinning the platform version alone would still have produced a
  # zero-test run. `junit/junit` also caches 3.8.1 here, which must not win;
  # picking the newest per artifact takes 4.13.2 and leaves 3.8.1 out.
  cp_add_newest_artifact "$m2/junit/junit"
  cp_add_newest_artifact "$m2/org/hamcrest/hamcrest-core"

  local a
  for a in junit-platform-commons junit-platform-engine junit-platform-launcher \
           junit-platform-suite-api; do
    cp_add_newest_artifact "$m2/org/junit/platform/$a"
  done
  for a in junit-jupiter junit-jupiter-api junit-jupiter-engine junit-jupiter-params; do
    cp_add_newest_artifact "$m2/org/junit/jupiter/$a"
  done
  cp_add_newest_artifact "$m2/org/junit/vintage/junit-vintage-engine"
  cp_add_jars_r "$m2/org/opentest4j"
  cp_add_jars_r "$m2/org/apiguardian"
  return 0
}

corpus_discover() {
  local r="$1" f rel
  find "$r/output/testclasses" -name 'Test*.class' 2>/dev/null | sort | while IFS= read -r f; do
    case "$f" in *'$'*) continue ;; esac
    rel="${f#"$r/output/testclasses/"}"
    rel="${rel%.class}"
    rel="${rel//\//.}"
    # `Tester*` is Tomcat's naming convention for a test FIXTURE -- a support
    # class, a fake OCSP responder, a valve that records what it was given --
    # not a test. Tomcat's own build agrees and says so explicitly:
    # build.xml:2276 carries `<exclude name="**/Tester*.java" />` inside the
    # fileset that selects what to run. `Test*` matches `Tester*`, so without
    # this filter 152 of the 805 names discovered here (19%) are classes that
    # declare no test at all. Running one is not a VM measurement: it produces
    # an empty or erroring JUnit result on BOTH arms and pollutes the ratio
    # with rows that say nothing about CratonVM.
    case "${rel##*.}" in Tester*) continue ;; esac
    printf '%s\n' "$rel"
  done
}
