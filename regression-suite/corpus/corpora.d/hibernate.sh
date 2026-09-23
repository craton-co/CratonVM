# Hibernate ORM 8.0.0-SNAPSHOT.
#
# THE DUMPED CLASSPATH FILE IS STALE, and silently so. The corpus root carries
# a 40174-byte `cratonvm-test-classpath.txt` whose every line is an absolute
# path under
#
#     C:\craton\CratonVM\apps\hibernate-orm\...
#
# and there is NO hibernate-orm under /data/cratonvm/apps on this host --
# only a `hib-suite-runner`. Hibernate exists at /data/cratonvm/apps/hibernate-orm
# and nowhere else. So the file resolves to ~438 non-existent jars. Because
# `cp_add` skips entries that do not exist, feeding it raw would produce a
# nearly EMPTY classpath and a run of identical NoClassDefFoundErrors.
#
# This definition rewrites the dead prefix onto the live root rather than
# trusting the file. That is a repair, not a verification: the rewrite is
# checked below by requiring that a decent number of the rewritten entries
# actually exist, and the corpus is marked `probable` because this lane
# composed the classpath but did not run a Hibernate workload.
#
# Second reason this corpus needs the @argfile path: at 40174 bytes the
# classpath is well past the 32767-character Windows command-line cap. Inline
# it would not fail cleanly -- it would fail as a TRUNCATED classpath, i.e. as
# a fabricated linkage error.

# MEASURED ON THIS HOST 2026-08-12, and the result is worse than a stale
# prefix. Of the 241 entries in cratonvm-test-classpath.txt, after the
# rewrite below, only 57 exist and 183 do not. The 183 are NOT hibernate
# paths -- they are gradle module-cache jars:
#
#   C:\Users\Victor\.gradle\caches\modules-2\files-2.1\jakarta.persistence\...
#   C:\Users\Victor\.gradle\caches\modules-2\files-2.1\org.wildfly.transaction\...
#
# i.e. the dependency jars have been EVICTED from the gradle cache since the
# classpath was dumped. No amount of path rewriting recovers them; the corpus
# needs a gradle resolve to repopulate the cache, which is a build and is
# therefore out of this lane's scope.
#
# So this corpus is BLOCKED, and the definition says so by failing loudly.
# The alternative -- composing the 57 surviving entries and running anyway --
# is the exact failure mode this whole driver exists to prevent: it would
# produce a run of identical NoClassDefFoundErrors that a reader would
# reasonably file as a VM regression.

CORPUS_DESC="Hibernate ORM 8.0.0-SNAPSHOT, hibernate-core + tests"
CORPUS_KIND=junit
CORPUS_CONFIDENCE=blocked
CORPUS_NOTE="BLOCKED: 183 of 241 classpath entries are evicted gradle-cache jars. Needs a gradle resolve, not a path fix."

CORPUS_ROOT_CANDIDATES="/data/cratonvm/apps/hibernate-orm"

CORPUS_RUNNER_CLASS="SbRunner"
CORPUS_RUNNER_SRC="/data/cratonvm/apps/spring-boot/sb-runner/SbRunner.java"
CORPUS_DEFAULT_CLASS=""

corpus_is_built() {
  local r="$1"
  [ -d "$r/hibernate-core/target/classes/java/main" ] || return 1
  [ -n "$(find "$r/hibernate-core/target/classes/java/main" -name '*.class' 2>/dev/null | head -1)" ] || return 1
  return 0
}

corpus_classpath() {
  local r="$1" f line p live=0 dead=0
  cp_add "$r/hibernate-core/target/classes/java/main"
  cp_add "$r/hibernate-core/target/classes/java/test"
  cp_add "$r/hibernate-core/target/resources/main"
  cp_add "$r/hibernate-core/target/resources/test"

  f="$r/cratonvm-test-classpath.txt"
  if [ -s "$f" ]; then
    while IFS= read -r line; do
      [ -n "$line" ] || continue
      p="${line%$'\r'}"
      p="${p//\\//}"                 # backslashes to forward slashes
      # Rewrite the recorded-but-dead root onto this host's live root.
      case "$p" in
        [Cc]:/craton/[Cc]ratonVM/apps/hibernate-orm/*|[Cc]:/craton/cratonvm/apps/hibernate-orm/*)
          p="$r/${p#*/apps/hibernate-orm/}" ;;
      esac
      if [ -e "$p" ]; then live=$((live+1)); cp_add "$p"; else dead=$((dead+1)); fi
    done < "$f"
    # A MOSTLY-dead classpath must be as loud as an entirely dead one. The
    # threshold is a majority, not a token count: with 57/241 surviving,
    # a `live > 0` check would have passed and handed the VM a classpath
    # missing jakarta.persistence.
    if [ "$dead" -gt "$live" ]; then
      echo "ERROR: hibernate classpath is not usable on this host: $live entries exist, $dead do not." >&2
      echo "       The missing ones are gradle module-cache jars that have been EVICTED since" >&2
      echo "       $f was dumped. This is a fixture problem: re-resolve the" >&2
      echo "       gradle dependencies and re-dump the classpath. Running with the survivors" >&2
      echo "       would manufacture NoClassDefFoundErrors that read as VM regressions." >&2
      return 1
    fi
  fi
  return 0
}

corpus_discover() {
  local r="$1" f rel base="$1/hibernate-core/target/classes/java/test"
  [ -d "$base" ] || return 0
  find "$base" -name '*Test.class' 2>/dev/null | sort | while IFS= read -r f; do
    case "$f" in *'$'*) continue ;; esac
    rel="${f#"$base/"}"; rel="${rel%.class}"
    printf '%s\n' "${rel//\//.}"
  done
}
