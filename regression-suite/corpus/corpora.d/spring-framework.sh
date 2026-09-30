# Spring Framework 7.1.0-SNAPSHOT. Verified built on this host 2026-08-12.
#
# Only ONE root has it: /data/cratonvm/apps/spring-framework.
# /data/cratonvm/apps/spring-framework does not exist. The two corpus trees on this
# host are complementary, not mirrored.
#
# Layout: a gradle multi-module build. Each module publishes
#   <module>/build/libs/<module>-7.1.0-SNAPSHOT.jar
# and also leaves <module>/build/classes/java/{main,test,testFixtures} behind.
# The jars are preferred for the module's MAIN output: they are the artefact
# Spring itself ships. The workloads themselves are only in the classes trees.
#
# TWO REPACKED JARS ARE LOAD-BEARING and are easy to miss because they are not
# named after a module:
#   spring-core/build/libs/spring-javapoet-repack-0.10.0.jar
#   spring-core/build/libs/spring-objenesis-repack-3.5.jar
# Spring relocates these dependencies into its own namespace, so nothing else
# on the classpath provides them and their absence surfaces at RUNTIME as a
# NoClassDefFoundError inside org.springframework.*, which reads as a VM bug.
#
# ---------------------------------------------------------------------------
# 2026-08-12 (lane B2): the classpath this file composed COULD NOT RUN A SINGLE
# SPRING TEST, on HotSpot or anywhere else. Three separate defects, all of
# which surfaced as linkage errors inside org.springframework.* and all of
# which would have been misread as VM findings:
#
#   1. NO commons-logging. Spring 7 dropped the vendored `spring-jcl` and
#      compiles against org.apache.commons.logging directly, so EVERY class
#      that declares a `Log` field -- which is most of them -- died with
#      NoClassDefFoundError: org/apache/commons/logging/LogFactory. Not
#      vendored anywhere under the spring tree; taken from the gradle cache.
#
#   2. NO TEST FIXTURES. Spring's tests are written against per-module fixture
#      classes (org.springframework.beans.testfixture.beans.TestBean and
#      friends) which live in build/classes/java/testFixtures, a source set
#      this file did not add. The previous header excluded the
#      `-test-fixtures.jar` variants on the theory that they drag in absent
#      optional dependencies. That theory is wrong in the way that matters:
#      the fixtures are not optional, they are the test's own vocabulary, and
#      a missing optional dependency inside one fixture only fails when that
#      fixture is loaded, whereas their wholesale absence fails everything.
#      The classes trees are used rather than the jars so that a fixture
#      resource directory can be added alongside.
#
#   3. TWO JUNIT MAJORS AT ONCE. It pulled every jar under the local maven
#      repo's org/junit, which on this host is jupiter 5.14.4 + platform
#      1.12.1 AND 1.14.4 AND the vintage engine. Spring 7.1 is built against
#      JUnit 6.1.0 (platform 6.x). Whichever of three platform-commons jars
#      won the classpath order decided whether the launcher could see the
#      engine at all. Third-party jars are now PINNED, by exact coordinate,
#      to the versions gradle resolved for this build, and a missing one
#      REFUSES the classpath instead of silently composing a shorter one.
#
# Also dropped: buildSrc/build/libs/buildSrc.jar (gradle build logic, not part
# of the application) and build/libs/spring-7.1.0-SNAPSHOT.jar (the root
# aggregate, which contains exactly a manifest and nothing else).
#
# SCOPE OF THE PINNED DEPENDENCY SET. It is sized for the five modules that
# make up a Spring ApplicationContext -- spring-core, spring-beans,
# spring-context, spring-aop, spring-expression -- plus the JUnit/AssertJ/
# Mockito harness they are written against. Tests in spring-web,
# spring-webmvc, spring-webflux, spring-messaging, spring-orm, spring-oxm,
# spring-jms and spring-test additionally need jackson, netty, tomcat, jaxb,
# hibernate and more, which are NOT pinned here. Those classes are still
# discoverable; they will fail to load on BOTH arms, which the driver reports
# as ORACLE-UNUSABLE rather than as a CratonVM result. Extend the pin list
# rather than reading such a row as a finding.

CORPUS_DESC="Spring Framework 7.1.0-SNAPSHOT, module jars + test/testFixtures classes"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="junit-kind: JUnit 6.1.0 platform launcher, pinned from the gradle cache (see SPRING_TEST_DEPS)"

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/spring-framework"

# Spring test classes have no main. They are driven through the JUnit
# Platform launcher. Rather than write a third copy of that launcher, this
# reuses the one the Spring Boot lane already wrote and debugged, which emits
# the machine-parseable SBRUNNER_RESULT line this driver's oracle-vacuity
# check reads (tests=0, or aborted==tests, is a disagreeing precondition and
# is never scored green).
CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="/data/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"

# Deliberately EMPTY. Spring has thousands of test classes and no obvious
# canonical one; picking a default here would smuggle in a claim about which
# of them is expected to pass. Use `discover` and `--classes-from`.
CORPUS_DEFAULT_CLASS=""

# Third-party jars, group:artifact:version, resolved out of the gradle module
# cache. Pinned rather than globbed: see defect 3 in the header. Verified
# present and verified sufficient to run spring-core / spring-beans /
# spring-context / spring-aop / spring-expression tests on HotSpot 25 on
# 2026-08-12.
#
# TWO OF THESE ARE NON-OBVIOUS AND EACH COST A FALSE LEAD:
#   org.jspecify:jspecify -- Spring 7 annotates with JSpecify @Nullable and
#     DefaultListableBeanFactory.doResolveDependency READS that annotation at
#     runtime, so its absence is not a compile-only concern: every
#     @Autowired-constructor injection dies with
#     NoClassDefFoundError: org/jspecify/annotations/Nullable.
#   org.aspectj:aspectjweaver -- @EnableAspectJAutoProxy, and hence anything
#     that turns on Spring AOP, instantiates ReflectiveAspectJAdvisorFactory,
#     which needs org.aspectj.lang.annotation.Pointcut. Without it the
#     failure surfaces on the internalAutoProxyCreator bean, which reads like
#     a Spring/VM defect and is neither.
SPRING_TEST_DEPS="
commons-logging:commons-logging:1.3.5
org.jspecify:jspecify:1.0.0
org.aspectj:aspectjweaver:1.9.25
org.aspectj:aspectjrt:1.9.25
org.junit.platform:junit-platform-commons:6.1.0
org.junit.platform:junit-platform-engine:6.1.0
org.junit.platform:junit-platform-launcher:6.1.0
org.junit.platform:junit-platform-suite:6.1.0
org.junit.platform:junit-platform-suite-api:6.1.0
org.junit.platform:junit-platform-suite-engine:6.1.0
org.junit.platform:junit-platform-testkit:6.1.0
org.junit.jupiter:junit-jupiter:6.1.0
org.junit.jupiter:junit-jupiter-api:6.1.0
org.junit.jupiter:junit-jupiter-engine:6.1.0
org.junit.jupiter:junit-jupiter-params:6.1.0
org.opentest4j:opentest4j:1.3.0
org.apiguardian:apiguardian-api:1.1.2
org.assertj:assertj-core:3.27.7
org.mockito:mockito-core:5.23.0
org.mockito:mockito-junit-jupiter:5.23.0
net.bytebuddy:byte-buddy:1.18.8
net.bytebuddy:byte-buddy-agent:1.18.8
org.objenesis:objenesis:3.5
jakarta.annotation:jakarta.annotation-api:3.0.0
jakarta.inject:jakarta.inject-api:2.0.1
io.projectreactor:reactor-core:3.8.6
io.projectreactor:reactor-test:3.8.6
org.reactivestreams:reactive-streams:1.0.4
org.hamcrest:hamcrest:3.0
org.xmlunit:xmlunit-core:2.10.4
org.xmlunit:xmlunit-assertj:2.10.4
io.micrometer:micrometer-observation:1.16.6
io.micrometer:micrometer-commons:1.16.6
io.micrometer:context-propagation:1.2.1
org.yaml:snakeyaml:2.6
jakarta.validation:jakarta.validation-api:3.1.0
jakarta.el:jakarta.el-api:6.0.1
jakarta.persistence:jakarta.persistence-api:3.2.0
org.hibernate.validator:hibernate-validator:9.1.0.Final
org.apache.groovy:groovy:5.0.6
org.apache.groovy:groovy-jsr223:5.0.6
org.apache.groovy:groovy-xml:5.0.6
org.slf4j:slf4j-api:2.0.17
com.thoughtworks.qdox:qdox:2.2.0
"

corpus_is_built() {
  local r="$1"
  [ -n "$(find "$r/spring-core/build/libs" -name 'spring-core-*.jar' 2>/dev/null | head -1)" ] || return 1
  [ -n "$(find "$r/spring-beans/build/libs" -name 'spring-beans-*.jar' 2>/dev/null | head -1)" ] || return 1
  # The workloads live in the classes trees, not the jars. A root with jars
  # and no compiled tests is a publish-only build and cannot be run.
  [ -d "$r/spring-beans/build/classes/java/test" ] || return 1
  return 0
}

corpus_classpath() {
  local r="$1" j m
  # Module MAIN jars. The `-test-fixtures` variants are skipped because the
  # equivalent classes tree is added below and carries its resources with it;
  # buildSrc is gradle build logic and the root aggregate jar is empty.
  while IFS= read -r j; do
    [ -n "$j" ] || continue
    case "$j" in
      *-test-fixtures.jar)  continue ;;
      */buildSrc/build/*)   continue ;;
      "$r"/build/libs/*)    continue ;;
    esac
    cp_add "$j"
  done < <(find "$r" -path '*/build/libs/*.jar' 2>/dev/null | sort)

  # Compiled test classes, test fixtures, and both resource sets, per module.
  for m in "$r"/spring-* "$r"/integration-tests; do
    [ -d "$m" ] || continue
    cp_add "$m/build/classes/java/test"
    cp_add "$m/build/classes/java/testFixtures"
    cp_add "$m/build/resources/test"
    cp_add "$m/build/resources/testFixtures"
  done

  # Pinned third-party jars out of the gradle module cache. A missing one is
  # REFUSED, not skipped: cp_add drops a non-existent entry silently, and a
  # silently shorter classpath is exactly how a fixture defect gets reported
  # as a VM linkage bug.
  local gc="${GRADLE_CACHE:-C:/Users/Victor/.gradle/caches/modules-2/files-2.1}"
  local spec g a v jar missing=""
  for spec in $SPRING_TEST_DEPS; do
    g="${spec%%:*}"; a="${spec#*:}"; v="${a##*:}"; a="${a%%:*}"
    jar="$(find "$gc/$g/$a/$v" -name "$a-$v.jar" 2>/dev/null | head -1)"
    if [ -z "$jar" ]; then missing="$missing $g:$a:$v"; continue; fi
    cp_add "$jar"
  done
  if [ -n "$missing" ]; then
    echo "ERROR: spring-framework: pinned third-party jars absent from $gc:" >&2
    for spec in $missing; do echo "         $spec" >&2; done
    echo "       Refusing to compose a partial classpath -- the resulting" >&2
    echo "       NoClassDefFoundError would read as a CratonVM defect." >&2
    return 1
  fi
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
