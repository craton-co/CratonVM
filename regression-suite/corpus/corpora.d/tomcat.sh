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
# so the classpath composes straight off disk. Only C:/craton/apps/tomcat has
# this; there is no tomcat under C:/craton/cratonvm/apps (only a
# tomcat-suite-runner, which is a runner, not a corpus -- the presence of a
# `*-suite-runner` directory says nothing about whether the corpus is built).

CORPUS_DESC="Apache Tomcat 12 server classes + its own test suite"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="ant build NOT required; output/ is already populated on this host"

CORPUS_ROOT_CANDIDATES="C:/craton/apps/tomcat"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="C:/craton/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"

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

corpus_classpath() {
  local r="$1"
  cp_add "$r/output/classes"
  cp_add "$r/output/testclasses"
  cp_add_jars "$r/output/build/lib"
  # Tomcat's own third-party test dependencies (junit, easymock, ...) live
  # alongside the build, not in output/build/lib.
  cp_add_jars_r "$r/output/build/webapps/examples/WEB-INF/lib"
  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"
  cp_add_jars_r "$m2/org/junit/platform"
  cp_add_jars_r "$m2/org/junit/jupiter"
  cp_add_jars_r "$m2/org/junit/vintage"
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
    printf '%s\n' "${rel//\//.}"
  done
}
