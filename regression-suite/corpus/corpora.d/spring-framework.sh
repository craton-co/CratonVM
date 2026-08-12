# Spring Framework 7.1.0-SNAPSHOT. Verified built on this host 2026-08-12.
#
# Only ONE root has it: C:/craton/cratonvm/apps/spring-framework.
# C:/craton/apps/spring-framework does not exist. The two corpus trees on this
# host are complementary, not mirrored.
#
# Layout: a gradle multi-module build. Each module publishes
#   <module>/build/libs/<module>-7.1.0-SNAPSHOT.jar
# and also leaves <module>/build/classes/java/{main,test} behind. The jars are
# preferred: they are the artefact Spring itself ships, and the classes trees
# additionally contain the module's own test fixtures.
#
# TWO REPACKED JARS ARE LOAD-BEARING and are easy to miss because they are not
# named after a module:
#   spring-core/build/libs/spring-javapoet-repack-0.10.0.jar
#   spring-core/build/libs/spring-objenesis-repack-3.5.jar
# Spring relocates these dependencies into its own namespace, so nothing else
# on the classpath provides them and their absence surfaces at RUNTIME as a
# NoClassDefFoundError inside org.springframework.*, which reads as a VM bug.
#
# `-test-fixtures` jars are excluded by default: they are compiled against the
# module's internal test scaffolding and pull in optional dependencies that
# are genuinely not present here, so including them converts a missing
# optional dependency into a fake linkage failure.

CORPUS_DESC="Spring Framework 7.1.0-SNAPSHOT, all published module jars"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="junit-kind: needs a JUnit platform launcher on the classpath (see CORPUS_RUNNER_SRC)"

CORPUS_ROOT_CANDIDATES="C:/craton/cratonvm/apps/spring-framework"

# Spring test classes have no main. They are driven through the JUnit
# Platform launcher. Rather than write a third copy of that launcher, this
# reuses the one the Spring Boot lane already wrote and debugged, which emits
# the machine-parseable SBRUNNER_RESULT line this driver's oracle-vacuity
# check reads (tests=0, or aborted==tests, is a disagreeing precondition and
# is never scored green).
CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="C:/craton/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"

# Deliberately EMPTY. Spring has thousands of test classes and no obvious
# canonical one; picking a default here would smuggle in a claim about which
# of them is expected to pass. Use `discover` and `--classes-from`.
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -n "$(find "$r/spring-core/build/libs" -name 'spring-core-*.jar' 2>/dev/null | head -1)" ] || return 1
  [ -n "$(find "$r/spring-beans/build/libs" -name 'spring-beans-*.jar' 2>/dev/null | head -1)" ] || return 1
  return 0
}

corpus_classpath() {
  local r="$1" j m
  # Module jars, minus the test-fixtures variants (see header).
  while IFS= read -r j; do
    [ -n "$j" ] || continue
    case "$j" in *-test-fixtures.jar) continue ;; esac
    cp_add "$j"
  done < <(find "$r" -path '*/build/libs/*.jar' 2>/dev/null | sort)

  # Compiled test classes and their resources, per module: the workloads
  # themselves live here, not in the published jars.
  for m in "$r"/spring-*; do
    [ -d "$m" ] || continue
    cp_add "$m/build/classes/java/test"
    cp_add "$m/build/resources/test"
  done

  # The JUnit platform launcher + engines. Not part of Spring's own output;
  # taken from the local maven repository, which is where the other app
  # runners on this host get them too.
  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"
  cp_add_jars_r "$m2/org/junit/platform"
  cp_add_jars_r "$m2/org/junit/jupiter"
  cp_add_jars_r "$m2/org/opentest4j"
  cp_add_jars_r "$m2/org/apiguardian"
  return 0
}

corpus_discover() {
  local r="$1" f rel m
  for m in "$r"/spring-*; do
    [ -d "$m/build/classes/java/test" ] || continue
    find "$m/build/classes/java/test" -name '*Tests.class' 2>/dev/null | sort | while IFS= read -r f; do
      case "$f" in *'$'*) continue ;; esac
      rel="${f#"$m/build/classes/java/test/"}"
      rel="${rel%.class}"
      printf '%s\n' "${rel//\//.}"
    done
  done
}
