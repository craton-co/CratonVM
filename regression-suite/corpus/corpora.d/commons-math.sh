# Apache Commons Math 4 (commons-math4-*-SNAPSHOT). Built on this host.
#
# Maven multi-module: every module leaves target/classes and target/test-classes.
# Verified: commons-math-legacy/target/classes holds 782 .class files, and all
# six modules have a populated target/test-classes.
#
# TWO measured corrections to the original note in this file (2026-08-12, B7):
#
# 1. `cp.txt` is NOT live. It names junit-jupiter 5.14.2 / junit-platform
#    1.14.2; this host now carries 5.14.4 / 1.14.4, and **5 of its 7 entries do
#    not exist**. Only opentest4j and apiguardian survive. It is still read,
#    because it is the only record of what the maven build resolved, but every
#    JUnit-family entry is skipped (see below) so a stale version in it can
#    never win the classpath.
#
# 2. SUPERSEDED 2026-09-22: the commons-math dependency jars proper --
#    commons-numbers-*, commons-rng-*, commons-statistics-*, commons-geometry-*
#    -- were reported gone from this host entirely on 2026-08-12. They are
#    BACK (or this is a different host with the same corpus checked out):
#    every one of the 19 artifacts `corpus_classpath` below now lists resolves
#    under C:/Users/Victor/.m2/repository. Their continued absence from THIS
#    FUNCTION (fixed 2026-09-22, not merely from the .m2 repo) was independently
#    producing five false DIVERGE verdicts -- AbstractIntegerDistributionTest,
#    SingularValueDecompositionTest, MultivariateSummaryStatisticsTest,
#    StatisticsTest, SynchronizedMultivariateSummaryStatisticsTest -- because
#    HotSpot and CratonVM resolve a class referenced by not-yet-executed code
#    with different eagerness, so the SAME missing class produced a different
#    tests=/failed= shape on each arm (see corpus_classpath's own comment).
#    Re-verified with the completed classpath: all five now score AGREE, and a
#    full `run` sweep of the corpus (2026-09-22) found no CratonVM-only
#    residual -- see
#    docs/internal/fixed-suite-bugs/bug-commonsmath-five-divergences-were-a-missing-classpath-jar-FIXED-20260922.md
#    for the write-up. If a future run
#    finds these artifacts gone again, restore the ORACLE-UNUSABLE framing
#    below rather than re-deleting the classpath entries: a corpus that stops
#    adding a real dependency because it once was missing is how this bug came
#    back.
#
#    Every module except commons-math-legacy-exception declares
#    commons-numbers-core, so before this fix most of the 350 discovered test
#    classes could not link on a host that genuinely lacks these jars. On such
#    a host this is not a failure to mark: the HotSpot oracle fails on them
#    identically and run-corpus.sh reports ORACLE-UNUSABLE, the correct, honest
#    verdict. `discover-runnable` below is the subset worth spending wall time
#    on in that case.

CORPUS_DESC="Apache Commons Math 4, all modules + tests"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=partial
CORPUS_NOTE="2026-10-10, default (--jdk-only) mode, CORPUS_RNG_SEED=1, the 325 non-abstract test classes: AGREE=325, 3765 tests, the 44 HotSpot failures identical test for test. The 25 *AbstractTest classes have no tests (ORACLE-VACUOUS by design); JdkMathTest's one test is aborted by an assumption on both arms. Its own dependency jars (commons-numbers/rng/statistics) were EVICTED from the Azure host the 2026-08-12 note was written on; this Windows host carries them (verified 2026-09-22)."

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/commons-math /data/cratonvm/apps/commons-math C:/craton/cratonvm/apps/commons-math"

CORPUS_RUNNER_CLASS="SbRunner"
# The checked-in runner first: the apps/spring-boot copy is not in version control and was absent on the
# hosts this corpus was last run on (2026-10-10).
CORPUS_RUNNER_SRC="$HERE/runners/SbRunner.java /data/cratonvm/apps/spring-boot/sb-runner/SbRunner.java C:/craton/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"

# ~35 tests draw UNSEEDED random input (commons-rng `RandomSource.X.create()`) and fail on HotSpot itself at
# 1/37..37/37 (measured 2026-10-10), so two correct VMs diverged on a coin flip. corpus_classpath puts a
# deterministic SeedFactory first on BOTH arms' classpath, which makes every run a pure function of this
# number: every test is compared, failure messages included. Change the seed to explore other inputs; a
# DIVERGE under any seed is a CratonVM finding reproducible with that seed. Exported so both arms see it.
export CORPUS_RNG_SEED="${CORPUS_RNG_SEED:-1}"
CORPUS_DEFAULT_CLASS="org.apache.commons.math4.core.jdkmath.JdkMathTest"

corpus_is_built() {
  local r="$1"
  [ -d "$r/commons-math-legacy/target/classes" ] || return 1
  [ -n "$(find "$r/commons-math-legacy/target/classes" -name '*.class' 2>/dev/null | head -1)" ] || return 1
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
# the markers match and run-corpus.sh scored the class **AGREE** -- measured
# here on 2026-08-12, on this corpus and on bc-java.
#
# The run-corpus.sh half of the fix LANDED on 2026-08-12 (lane C8):
# `oracle_vacuous` now returns vacuous for a junit-kind oracle that threw with
# no SBRUNNER_RESULT at all (it died during DISCOVERY) and for a
# linkage-family throw escaping to the wrapper. Either shape is reported as
# ORACLE-VACUOUS -- unadjudicated -- instead of AGREE. See
# docs/known-issues/jdk-only/C8-CORPUS-HARNESS-DEFECTS-20260812.md §2.8.
# Re-adjudicating the stored logs with that guard (lane C17) moved the two
# `AccurateMathTest`/`DfpTest` rows in this corpus off DIVERGE and onto
# ORACLE-VACUOUS, which is the honest reading: the oracle is the arm that died.
# The classpath half below is still what stops the failure happening at all.
#
# The classpath half of the fix: pin ONE jar per artifact, the highest version
# present, instead of adding a whole group directory recursively.
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
  # commons-math's poms declare junit-vintage-engine in every module: a large
  # part of this corpus is still JUnit 4.
  cp_add_latest_jar "$m2/org/junit/vintage/junit-vintage-engine"
  cp_add_latest_jar "$m2/junit/junit"
  cp_add_latest_jar "$m2/org/hamcrest/hamcrest"
  cp_add_latest_jar "$m2/org/hamcrest/hamcrest-core"
  cp_add_latest_jar "$m2/org/opentest4j/opentest4j"
  cp_add_latest_jar "$m2/org/apiguardian/apiguardian-api"
  return 0
}

corpus_classpath() {
  local r="$1" m line j
  for m in "$r"/commons-math-*; do
    [ -d "$m" ] || continue
    cp_add "$m/target/classes"
    cp_add "$m/target/test-classes"
  done

  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"
  # Aligned JUnit stack FIRST: a classpath is first-wins, so this must precede
  # cp.txt for the pin to actually hold.
  cp_add_junit_aligned "$m2"

  # cp.txt is a single ';'-joined line of absolute .m2 jar paths. JUnit-family
  # entries are skipped: they name 5.14.2/1.14.2, which are not on this host,
  # and if they ever come back they would re-create the mismatch above.
  if [ -s "$r/cp.txt" ]; then
    while IFS= read -r line; do
      line="${line%$'\r'}"
      while [ -n "$line" ]; do
        j="${line%%;*}"
        if [ "$j" = "$line" ]; then line=""; else line="${line#*;}"; fi
        [ -n "$j" ] || continue
        case "$j" in
          *junit*|*opentest4j*|*apiguardian*|*hamcrest*) continue ;;
        esac
        cp_add "${j//\\//}"
      done
    done < "$r/cp.txt"
  fi

  # commons-math3 is a real declared dependency of commons-math-transform and
  # is one of the few that survives on this host.
  cp_add_latest_jar "$m2/org/apache/commons/commons-math3"

  # commons-numbers/rng/statistics/geometry: declared dependencies of
  # commons-math-legacy and friends (e.g. AbstractIntegerDistribution implements
  # org.apache.commons.statistics.distribution.DiscreteDistribution;
  # SingularValueDecompositionTest's test fixture pulls in commons-rng
  # transitively via EigenDecompositionTest). The file-header note above
  # ("EVICTED from this host") was true of the Azure box this corpus was first
  # run on; verified 2026-09-22 that all of these ARE present in
  # C:/Users/Victor/.m2/repository on this host. Their absence from this
  # function (not from the .m2 repo) is what produced five false DIVERGE rows
  # (AbstractIntegerDistributionTest, SingularValueDecompositionTest,
  # MultivariateSummaryStatisticsTest, StatisticsTest,
  # SynchronizedMultivariateSummaryStatisticsTest): with no commons-statistics/
  # commons-rng jar on the classpath, BOTH arms hit NoClassDefFoundError on the
  # SAME missing class -- but HotSpot and CratonVM resolve classes referenced
  # by not-yet-executed code with different eagerness (verified via each
  # class's .hs.log vs .cv.log: identical missing-class name, different
  # tests=/failed= shape, and for SingularValueDecompositionTest CratonVM's
  # lazier resolution never touches commons-rng at all and the class PASSES
  # outright). That eagerness difference is legal JVMS variance, not a defect;
  # it is invisible once the classpath is complete, which is the actual fix.
  for artifact in commons-numbers-core commons-numbers-angle commons-numbers-arrays \
                  commons-numbers-combinatorics commons-numbers-complex commons-numbers-field \
                  commons-numbers-fraction commons-numbers-gamma commons-numbers-quaternion \
                  commons-numbers-rootfinder commons-rng-client-api commons-rng-core \
                  commons-rng-sampling commons-rng-simple commons-statistics-descriptive \
                  commons-statistics-distribution commons-statistics-inference \
                  commons-statistics-ranking commons-geometry-core commons-geometry-euclidean; do
    cp_add_latest_jar "$m2/org/apache/commons/$artifact"
  done

  # Deterministic commons-rng seeds, FIRST on the classpath so it shadows the
  # jar's SeedFactory: see runners/rng-seed/.../SeedFactory.java and
  # CORPUS_RNG_SEED above. Compiled against the classpath just composed (it
  # needs commons-rng-core). Only when a javac is resolved -- the `run` path;
  # the classpath report of `info` does not need it.
  if [ -n "${JAVAC:-}" ]; then
    local sd="$BUILD/rng-seed"
    local src="$HERE/runners/rng-seed/org/apache/commons/rng/simple/internal/SeedFactory.java"
    local cls="$sd/org/apache/commons/rng/simple/internal/SeedFactory.class"
    if [ ! -f "$cls" ] || [ "$src" -nt "$cls" ]; then
      mkdir -p "$sd" && "$JAVAC" -d "$sd" -cp "$CP" "$src" || return 1
    fi
    CP="$sd$SEP$CP"
  fi
  return 0
}

corpus_discover() {
  local r="$1" f rel m
  for m in "$r"/commons-math-*; do
    [ -d "$m/target/test-classes" ] || continue
    find "$m/target/test-classes" -name '*Test.class' 2>/dev/null | sort | while IFS= read -r f; do
      case "$f" in *'$'*) continue ;; esac
      rel="${f#"$m/target/test-classes/"}"
      rel="${rel%.class}"
      printf '%s\n' "${rel//\//.}"
    done
  done
}
