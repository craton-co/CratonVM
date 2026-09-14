# Bouncy Castle (bc-java). Built on this host: core/build/classes/java/{main,test}
# holds 3085 .class files.
#
# Note the corpus root is also littered with hand-written probe classes
# (BCStyleProbe.class, F2mProbe.class, ChmIsolated.class, ...) compiled
# directly into the root directory by earlier investigations. Those are NOT
# part of bc-java and are deliberately NOT put on the classpath here: a probe
# that shadows a real class name would silently change what the corpus means.

CORPUS_DESC="Bouncy Castle core (provider + lightweight API) and its tests"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="JUnit arm exercised 2026-08-12 (B7). Use the 18 'AllTests' aggregators as the workload list -- most '*Test' names are non-JUnit SimpleTests and correctly report tests=0. See docs/known-issues/jdk-only/P4A-CORPORA-20260812.md."

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/bc-java /data/cratonvm/apps/bc-java"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="/data/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -d "$r/core/build/classes/java/main" ] || return 1
  [ -n "$(find "$r/core/build/classes/java/main" -name '*.class' 2>/dev/null | head -1)" ] || return 1
  return 0
}

# --- the JUnit alignment trap ----------------------------------------------
#
# The JUnit jars in this host's maven repository are NOT version aligned:
# org/junit/platform carries BOTH 1.12.1 and 1.14.4, while org/junit/jupiter
# carries only 5.14.4. `cp_add_jars_r` adds every one of them, and `find |
# sort` puts 1.12.1 first, so 1.12.1 wins the classpath. The launcher's own
# ClasspathAlignmentChecker then throws
# `org.junit.platform.commons.JUnitException` (wrapping `NoSuchMethodError:
# EngineDiscoveryRequest.getOutputDirectoryCreator()`) before a single test is
# discovered.
#
# That is worse than a broken run. The failure is IDENTICAL on both arms, so
# the markers match and run-corpus.sh scores the class **AGREE** -- measured
# here on 2026-08-12, on this corpus and on commons-math. `oracle_vacuous`
# does not catch it: it inspects the SBRUNNER_RESULT line, and this failure
# never gets far enough to print one. See
# docs/known-issues/jdk-only/P4A-CORPORA-20260812.md for the run-corpus.sh
# half of the fix, which this lane may not edit.
#
# The classpath half: pin ONE jar per artifact, the highest version present,
# instead of adding a whole group directory recursively.
cp_add_latest_jar() {
  local artdir="$1" j
  [ -d "$artdir" ] || return 0
  j="$(find "$artdir" -name '*.jar' ! -name '*-sources.jar' ! -name '*-javadoc.jar' ! -name '*-tests.jar' 2>/dev/null | sort -V | tail -1)"
  [ -n "$j" ] && cp_add "$j"
  return 0
}

cp_add_junit_aligned() {
  local m2="$1" a
  for a in junit-platform-commons junit-platform-engine junit-platform-launcher junit-platform-suite-api; do
    cp_add_latest_jar "$m2/org/junit/platform/$a"
  done
  for a in junit-jupiter junit-jupiter-api junit-jupiter-engine junit-jupiter-params; do
    cp_add_latest_jar "$m2/org/junit/jupiter/$a"
  done
  # bc-java's own tests are overwhelmingly JUnit 3 `junit.framework.TestCase`
  # subclasses, so the vintage engine and junit 4.x are not optional here --
  # without them the jupiter engine discovers nothing and every class reports
  # tests=0, which run-corpus.sh correctly refuses to score.
  cp_add_latest_jar "$m2/org/junit/vintage/junit-vintage-engine"
  cp_add_latest_jar "$m2/junit/junit"
  cp_add_latest_jar "$m2/org/hamcrest/hamcrest"
  cp_add_latest_jar "$m2/org/hamcrest/hamcrest-core"
  cp_add_latest_jar "$m2/org/opentest4j/opentest4j"
  cp_add_latest_jar "$m2/org/apiguardian/apiguardian-api"
  return 0
}

corpus_classpath() {
  local r="$1" m
  for m in core prov pkix mail pg tls util; do
    cp_add "$r/$m/build/classes/java/main"
    cp_add "$r/$m/build/classes/java/test"
    cp_add "$r/$m/build/resources/main"
    cp_add "$r/$m/build/resources/test"
  done
  cp_add_jars_r "$r/libs"
  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"
  cp_add_junit_aligned "$m2"
  return 0
}

# Discovery emits `*Test.class` AND `AllTests.class`, and the second half is
# the one that matters.
#
# Measured 2026-08-12: of a 30-class sample of the `*Test` names, **21 reported
# `tests=0` on HotSpot** and were correctly refused as ORACLE-VACUOUS. That is
# not a harness fault -- most of bc-java's `*Test` classes are
# `org.bouncycastle.util.test.SimpleTest` subclasses, which are NOT JUnit at
# all. They carry a `main` and a `performTest()`, and the JUnit engines find
# nothing in them.
#
# Running them as CORPUS_KIND=main would be WORSE than not running them.
# `SimpleTest.perform()` catches every exception and `runTest()` merely PRINTS
# the outcome (SimpleTest.java:185-215); nothing throws and nothing sets a
# non-zero exit. So a main-kind arm reaches `CORPUS-END completed=true` on both
# VMs whether the cryptography was right or wrong, and the run would score a
# full sweep of AGREE having adjudicated nothing at all -- the exact failure
# this corpus driver was built to refuse. The pass/fail text lives only on
# stdout, which `arm_key` deliberately excludes because real applications print
# timestamps and temp paths there.
#
# bc-java's own answer is the `AllTests` aggregators: JUnit 3 `TestSuite`s that
# wrap the SimpleTests and assert on them (e.g.
# `org.bouncycastle.crypto.test.AllTests` -> `SimpleTestTest`, which is how the
# 211 `crypto.test` SimpleTests become adjudicable). There are 18 of them in
# `core`, and they are the recommended workload list for this corpus.
corpus_discover() {
  local r="$1" f rel m base
  for m in core prov pkix mail pg tls util; do
    base="$r/$m/build/classes/java/test"
    [ -d "$base" ] || continue
    find "$base" \( -name '*Test.class' -o -name 'AllTests.class' \) 2>/dev/null | sort | while IFS= read -r f; do
      case "$f" in *'$'*) continue ;; esac
      rel="${f#"$base/"}"; rel="${rel%.class}"
      printf '%s\n' "${rel//\//.}"
    done
  done
}

# The adjudicable subset: just the JUnit 3 aggregators.
corpus_discover_suites() {
  local r="$1" f rel m base
  for m in core prov pkix mail pg tls util; do
    base="$r/$m/build/classes/java/test"
    [ -d "$base" ] || continue
    find "$base" -name 'AllTests.class' 2>/dev/null | sort | while IFS= read -r f; do
      case "$f" in *'$'*) continue ;; esac
      rel="${f#"$base/"}"; rel="${rel%.class}"
      printf '%s\n' "${rel//\//.}"
    done
  done
}
