# H2 Database. Verified on this host 2026-08-12.
#
# THE TWO ROOTS ARE NOT INTERCHANGEABLE, and this is the single most
# expensive fact in this file:
#
#   /data/cratonvm/apps/h2database/h2   BUILT   1114 classes, 687 test-classes
#   /data/cratonvm/apps/h2database/h2            DECOY   target/classes holds exactly ONE
#                                                   file, META-INF/versions/21/org/h2/
#                                                   util/Utils21.class; there is no
#                                                   org/h2/Driver.class and
#                                                   target/test-classes is EMPTY.
#
# Both directories exist, both contain a `target/classes`, and a runner that
# resolved the corpus by directory EXISTENCE would pick whichever came first
# and then report ~218 identical NoClassDefFoundErrors that read exactly like
# a VM regression. `corpus_is_built` below tests for org/h2/Driver.class
# specifically, so the decoy can never win.
#
# The inverse asymmetry matters just as much: the dependency jars (`ext/`)
# exist ONLY under the NON-built root. So the classpath is a UNION across the
# two trees -- built classes from one, third-party jars from the other. Neither
# root alone composes a working classpath.
#
# There is no h2*.jar anywhere. The only two jars under the h2 tree are
# `.mvn/wrapper/maven-wrapper.jar` and `service/wrapper.jar`, neither of which
# is H2. Do not glob for one.

CORPUS_DESC="H2 Database engine + its own test suite (main-per-test-class)"
CORPUS_KIND=main
CORPUS_CONFIDENCE=verified
CORPUS_NOTE="built classes and dependency jars live in DIFFERENT roots; the classpath unions them"

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/h2database/h2 /data/cratonvm/apps/h2database/h2 C:/craton/h2root"

# TestBitStream was dry-run on HotSpot 25 on this host and exits 0. It is also
# a `unit` test, so it needs no database file and no writable temp state --
# the cheapest workload that still proves the whole chain (classpath, wrapper,
# both arms, adjudication) is wired.
CORPUS_DEFAULT_CLASS="org.h2.test.unit.TestBitStream"

corpus_is_built() {
  local r="$1"
  [ -f "$r/target/classes/org/h2/Driver.class" ] || return 1
  [ -f "$r/target/test-classes/org/h2/test/TestBase.class" ] || return 1
  return 0
}

corpus_classpath() {
  local r="$1" other
  cp_add "$r/target/classes"
  cp_add "$r/target/test-classes"
  # `ext/` from whichever candidate root has it -- see the header.
  for other in $CORPUS_ROOT_CANDIDATES; do
    cp_add_jars "$other/ext"
  done
  return 0
}

# H2 writes database files relative to the working directory, so every arm
# must run from the same place or the two arms are not running the same
# workload. Both arms get this directory.
corpus_workdir() { echo "$1"; }

# ...and the same directory is why the arms must not INHERIT each other's
# files. The CratonVM arm runs first; whatever it leaves in `data/` is the
# oracle's input. A carried-over store made `TestBackup` fail with
# `MVStoreException: Chunk 2 not found` while all three arms are green run
# alone (docs/known-issues/jdk-only/P4A-H2-DIVERGENCES-20260812.md §3c, and
# C8-H2-TESTBACKUP-SHARED-WORKDIR-20260812.md). run-corpus.sh removes these
# paths, relative to the workdir, before EACH arm.
#
# `data` is safe to delete and is confirmed scratch: H2's OWN .gitignore lists
# it, and nothing on the classpath comes from it.
#
# DO NOT ADD `ext` HERE, and do not add anything else without checking the same
# way. `ext/` is also in H2's .gitignore -- and it is where `corpus_classpath`
# above gets every third-party jar. Declaring it would make the driver delete
# the classpath before the first arm and produce ~200 identical
# NoClassDefFoundErrors, which is exactly the shape that reads as a sweeping VM
# regression. `temp` is a candidate (H2 tests write there too) but no run has
# measured it carrying over, and an unmeasured entry here is a silent `rm -rf`.
CORPUS_CLEAN_PATHS="data"

# H2's own convention: a runnable test is a CONCRETE top-level class extending
# TestBase or TestDb, each carrying its own `main`. Discovery therefore takes
# an intersection of two sources, and needs both:
#
#   * the BUILT class files decide membership, so a class that exists in src
#     and was never compiled cannot enter the list and then fail for that
#     reason alone (which would read as a VM defect);
#   * the SOURCE decides concreteness, because `public abstract class` and
#     `public class` are indistinguishable from a .class file name. Reading
#     the class files alone yields TestBase and TestDb -- the abstract bases,
#     which have no runnable main -- as if they were workloads.
#
# The source-side rule is the same one H2 uses on itself in TestAll.java and
# that apps/h2database-suite-runner/run-h2-suite.sh:99 mirrors.
#
# TestAll and TestAllJunit are excluded by name: TestAll is not a test, it is
# H2's whole-suite driver, and running it as a "workload" would launch the
# entire suite inside one arm under one timeout.
corpus_discover() {
  local r="$1" f rel src
  find "$r/target/test-classes/org/h2/test" -name 'Test*.class' 2>/dev/null | sort | while IFS= read -r f; do
    case "$f" in *'$'*) continue ;; esac      # nested/anonymous classes have no main
    rel="${f#"$r/target/test-classes/"}"
    rel="${rel%.class}"
    case "$rel" in
      org/h2/test/TestAll|org/h2/test/TestAllJunit) continue ;;
    esac
    src="$r/src/test/$rel.java"
    if [ -f "$src" ]; then
      grep -qE '^public class [A-Za-z0-9_]+ extends (TestBase|TestDb)\b' "$src" || continue
    fi
    printf '%s\n' "${rel//\//.}"
  done
}
