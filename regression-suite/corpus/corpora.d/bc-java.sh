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
CORPUS_CONFIDENCE=probable
CORPUS_NOTE="classes verified present; workload not exercised by this lane"

CORPUS_ROOT_CANDIDATES="C:/craton/apps/bc-java C:/craton/cratonvm/apps/bc-java"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="C:/craton/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -d "$r/core/build/classes/java/main" ] || return 1
  [ -n "$(find "$r/core/build/classes/java/main" -name '*.class' 2>/dev/null | head -1)" ] || return 1
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
  cp_add_jars_r "$m2/org/junit/platform"
  cp_add_jars_r "$m2/org/junit/jupiter"
  cp_add_jars_r "$m2/org/junit/vintage"
  cp_add_jars_r "$m2/junit"
  cp_add_jars_r "$m2/org/opentest4j"
  cp_add_jars_r "$m2/org/apiguardian"
  return 0
}

corpus_discover() {
  local r="$1" f rel m base
  for m in core prov pkix mail pg tls util; do
    base="$r/$m/build/classes/java/test"
    [ -d "$base" ] || continue
    find "$base" -name '*Test.class' 2>/dev/null | sort | while IFS= read -r f; do
      case "$f" in *'$'*) continue ;; esac
      rel="${f#"$base/"}"; rel="${rel%.class}"
      printf '%s\n' "${rel//\//.}"
    done
  done
}
