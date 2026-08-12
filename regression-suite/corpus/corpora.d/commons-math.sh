# Apache Commons Math 4 (commons-math4-*-SNAPSHOT). Built on this host.
#
# Maven multi-module: every module leaves target/classes and target/test-classes.
# Verified: commons-math-legacy/target/classes holds 782 .class files, and all
# six modules have a populated target/test-classes.
#
# The dependency jars are NOT under the corpus tree -- they are in the local
# maven repository, and the corpus records which ones in `cp.txt` at the root
# (junit-jupiter 5.14.2 and friends). That file's paths are LIVE on this host,
# unlike hibernate's (see hibernate.sh), so it is read directly.

CORPUS_DESC="Apache Commons Math 4, all modules + tests"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=probable
CORPUS_NOTE="classes verified present; the JUnit arm has not been exercised by this lane"

CORPUS_ROOT_CANDIDATES="C:/craton/apps/commons-math C:/craton/cratonvm/apps/commons-math"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="C:/craton/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -d "$r/commons-math-legacy/target/classes" ] || return 1
  [ -n "$(find "$r/commons-math-legacy/target/classes" -name '*.class' 2>/dev/null | head -1)" ] || return 1
  return 0
}

corpus_classpath() {
  local r="$1" m line
  for m in "$r"/commons-math-*; do
    [ -d "$m" ] || continue
    cp_add "$m/target/classes"
    cp_add "$m/target/test-classes"
  done
  # cp.txt is a single ';'-joined line of absolute .m2 jar paths.
  if [ -s "$r/cp.txt" ]; then
    local j
    while IFS= read -r line; do
      line="${line%$'\r'}"
      while [ -n "$line" ]; do
        j="${line%%;*}"
        if [ "$j" = "$line" ]; then line=""; else line="${line#*;}"; fi
        [ -n "$j" ] && cp_add "${j//\\//}"
      done
    done < "$r/cp.txt"
  fi
  local m2="${M2_REPO:-C:/Users/Victor/.m2/repository}"
  cp_add_jars_r "$m2/org/junit/platform"
  cp_add_jars_r "$m2/org/junit/jupiter"
  cp_add_jars_r "$m2/org/opentest4j"
  cp_add_jars_r "$m2/org/apiguardian"
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
