#!/bin/bash
# Lever matrix for the two open items in
# docs/known-issues/springboot/loader-zip-jit-only-failure-cluster-20260804.md
#
# Every verdict is repeated 3x: single runs on this shared host have already
# produced three false "this lever fixes it" readings.
exec > /tmp/zj2-results.txt 2>&1

BIN=/tmp/cratonvm-zj2
JH=$(ls -d /usr/lib/jvm/jdk-25* /usr/lib/jvm/java-25* /opt/jdk-25* 2>/dev/null | head -1)
echo "=== head ==="; cat /tmp/zj2-head.txt 2>/dev/null
echo "=== host ==="; cat /proc/loadavg; nproc; echo "JAVA_HOME=$JH"
echo "=== oracles ==="
ls -la /tmp/ziporacle.sh /tmp/yamloracle.sh 2>&1
echo "--- ziporacle.sh ---"; cat /tmp/ziporacle.sh 2>/dev/null
echo "--- yamloracle.sh ---"; cat /tmp/yamloracle.sh 2>/dev/null

cp -f "$BIN" /tmp/cratonvm-zipjit 2>/dev/null && echo "binary staged at /tmp/cratonvm-zipjit"

# A --nojit row is only meaningful if the oracle actually forwards extra args.
# If it does not, those rows silently duplicate the baseline and would read as
# "--nojit fails too", which is the opposite of the truth.
for o in /tmp/ziporacle.sh /tmp/yamloracle.sh; do
  if grep -q 'CRATONVM_EXTRA_ARGS' "$o" 2>/dev/null; then
    echo "OK: $o forwards CRATONVM_EXTRA_ARGS"
  else
    echo "WARNING: $o does NOT forward CRATONVM_EXTRA_ARGS - its nojit row is NOT a nojit run"
  fi
done

######################################################################
# STAGE 0 - a standalone probe for the YAML corruption.
#
# The real test builds a >4 MiB StringBuilder one line at a time, converts to
# UTF-8 and hands it to snakeyaml, which reports a parse error deep inside the
# document. If this reproduces, the oracle drops from a ~100 s suite run to a
# ~10 s probe and the whole bisect gets cheap. It does NOT reproduce on
# Windows, so this is the first time it is being asked on the failing host.
######################################################################
mkdir -p /tmp/yg && cd /tmp/yg || exit 9
cat > YamlGrow.java <<'JAVA'
import java.nio.charset.StandardCharsets;

public class YamlGrow {
    static final String LINE = "- some list entry\n";

    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : 4_194_304;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        String variant = args.length > 2 ? args[2] : "plain";

        for (int rep = 0; rep < reps; rep++) {
            String s = variant.equals("cat2") ? buildCat2(target) : build(target);
            int bad = verifyChars(s);
            if (bad >= 0) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=chars " + describe(s, bad));
                return;
            }
            byte[] b = s.getBytes(StandardCharsets.UTF_8);
            if (b.length != s.length()) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=utf8-length chars="
                        + s.length() + " bytes=" + b.length);
                return;
            }
            int badb = verifyBytes(b);
            if (badb >= 0) {
                System.out.println("PROBE-FAIL rep=" + rep + " stage=bytes " + describeBytes(b, badb));
                return;
            }
            System.out.println("rep " + rep + " ok len=" + s.length());
        }
        System.out.println("PROBE-OK " + reps + " reps target=" + target + " variant=" + variant);
    }

    /** Verbatim shape of OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb. */
    static String build(int target) {
        StringBuilder yaml = new StringBuilder();
        while (yaml.length() < target) {
            yaml.append(LINE);
        }
        return yaml.toString();
    }

    /**
     * Same loop with a cat-2 local whose slot is reused by a cat-1 local in a
     * disjoint range - the exact shape behind the Arrays.sort OSR miscompile.
     */
    static String buildCat2(int target) {
        StringBuilder yaml = new StringBuilder();
        long checksum = 0;
        while (yaml.length() < target) {
            yaml.append(LINE);
            checksum += yaml.length();
        }
        int i = (int) (checksum & 0xff);
        if (i == -1) {
            yaml.append('x');
        }
        return yaml.toString();
    }

    static int verifyChars(String s) {
        int n = LINE.length();
        if (s.length() % n != 0) return s.length() - (s.length() % n);
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) != LINE.charAt(i % n)) return i;
        }
        return -1;
    }

    static int verifyBytes(byte[] b) {
        int n = LINE.length();
        for (int i = 0; i < b.length; i++) {
            if (b[i] != (byte) LINE.charAt(i % n)) return i;
        }
        return -1;
    }

    static String describe(String s, int bad) {
        int from = Math.max(0, bad - 40), to = Math.min(s.length(), bad + 40);
        return "firstBadChar=" + bad + " line=" + (bad / LINE.length())
                + " ctx=[" + s.substring(from, to).replace("\n", "\\n") + "]"
                + " got=" + (int) s.charAt(bad)
                + " want=" + (int) LINE.charAt(bad % LINE.length())
                + " totalLen=" + s.length();
    }

    static String describeBytes(byte[] b, int bad) {
        StringBuilder sb = new StringBuilder();
        for (int i = Math.max(0, bad - 40); i < Math.min(b.length, bad + 40); i++) {
            sb.append((char) (b[i] & 0xff));
        }
        return "firstBadByte=" + bad + " line=" + (bad / LINE.length())
                + " ctx=[" + sb.toString().replace("\n", "\\n") + "] total=" + b.length;
    }
}
JAVA
if [ -n "$JH" ] && "$JH/bin/javac" -d /tmp/yg YamlGrow.java 2>/tmp/yg/javac.err; then
  echo "probe compiled"
  echo "--- HotSpot control ---"
  "$JH/bin/java" -cp /tmp/yg YamlGrow 4194304 2 plain 2>&1 | tail -2
  for v in plain cat2; do
    for arm in "jit:" "nojit:--nojit"; do
      name="${arm%%:*}"; extra="${arm#*:}"
      for i in 1 2 3; do
        out=$(timeout 600 "$BIN" --java-home "$JH" --Xmx 2g $extra -cp /tmp/yg YamlGrow 4194304 2 $v 2>&1 | grep -E "PROBE-(OK|FAIL)" | tail -1)
        echo "[probe/$v/$name] rep$i :: ${out:-NORESULT}"
      done
    done
  done
else
  echo "PROBE COMPILE FAILED - treat every probe row below as absent, not as a pass"
  cat /tmp/yg/javac.err 2>/dev/null | head -5
fi

######################################################################
# STAGE 0.5 - which SIDE is corrupt?
#
# The cluster page asserts "the input snakeyaml is handed is already corrupt"
# because the test body is only the append loop. That does not follow:
# snakeyaml's own parser is JIT-compiled in the same run, and denying a
# snakeyaml method (constructSequenceStep2) was ALSO observed to make it pass.
# This probe verifies the 4 MiB document byte-for-byte BEFORE parsing it, so
# the build side and the parse side are separated by measurement.
######################################################################
CP=$(grep -oE '\-cp +[^ ]+' /tmp/yamloracle.sh 2>/dev/null | head -1 | awk '{print $2}')
[ -z "$CP" ] && CP=$(grep -oE 'CLASSPATH=[^ ]+' /tmp/yamloracle.sh 2>/dev/null | head -1 | cut -d= -f2)
echo "derived CP=${CP:-<none>}"
if [ -n "$CP" ] && [ -n "$JH" ]; then
  cat > /tmp/yg/YamlSplit.java <<'JAVA'
import java.nio.charset.StandardCharsets;

/** Separates "the built document is corrupt" from "the parser is miscompiled". */
public class YamlSplit {
    static final String LINE = "- some list entry\n";
    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : 4_194_304;
        StringBuilder yaml = new StringBuilder();
        while (yaml.length() < target) yaml.append(LINE);
        String s = yaml.toString();

        int n = LINE.length(), bad = -1;
        for (int i = 0; i < s.length() && bad < 0; i++)
            if (s.charAt(i) != LINE.charAt(i % n)) bad = i;
        if (bad >= 0) {
            System.out.println("SPLIT-RESULT build=CORRUPT firstBad=" + bad
                    + " line=" + (bad / n) + " len=" + s.length());
            return;
        }
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        int badb = -1;
        for (int i = 0; i < b.length && badb < 0; i++)
            if (b[i] != (byte) LINE.charAt(i % n)) badb = i;
        if (badb >= 0) {
            System.out.println("SPLIT-RESULT build=OK utf8=CORRUPT firstBad=" + badb
                    + " line=" + (badb / n));
            return;
        }
        System.out.println("SPLIT-INFO build=OK utf8=OK len=" + s.length()
                + " lines=" + (s.length() / n) + " - handing to snakeyaml");
        try {
            Object yamlObj = Class.forName("org.yaml.snakeyaml.Yaml")
                    .getDeclaredConstructor().newInstance();
            Object loaded = yamlObj.getClass()
                    .getMethod("load", String.class).invoke(yamlObj, s);
            int size = (loaded instanceof java.util.List) ? ((java.util.List<?>) loaded).size() : -1;
            System.out.println("SPLIT-RESULT build=OK parse=OK entries=" + size);
        } catch (Throwable t) {
            Throwable r = t; while (r.getCause() != null) r = r.getCause();
            System.out.println("SPLIT-RESULT build=OK parse=THREW " + r);
        }
    }
}
JAVA
  if "$JH/bin/javac" -cp "$CP" -d /tmp/yg /tmp/yg/YamlSplit.java 2>/tmp/yg/split.err; then
    echo "--- HotSpot control ---"
    "$JH/bin/java" -cp "$CP:/tmp/yg" YamlSplit 4194304 2>&1 | grep SPLIT | tail -2
    for arm in "jit:" "nojit:--nojit"; do
      name="${arm%%:*}"; extra="${arm#*:}"
      for i in 1 2 3; do
        out=$(timeout 900 "$BIN" --java-home "$JH" --Xmx 2g $extra -cp "$CP:/tmp/yg" YamlSplit 4194304 2>&1 | grep SPLIT | tail -1)
        echo "[split/$name] rep$i :: ${out:-NORESULT}"
      done
    done
  else
    echo "SPLIT PROBE COMPILE FAILED (snakeyaml not on derived CP) - rows absent, not passing"
    head -3 /tmp/yg/split.err
  fi
else
  echo "SPLIT PROBE SKIPPED - could not derive a classpath from the oracle"
fi

run3() {   # run3 <label> <oracle> <arg> [env assignments...]
  local label="$1"; shift
  local oracle="$1"; shift
  local arg="$1"; shift
  local i out ld
  for i in 1 2 3; do
    ld=$(cut -d' ' -f1 /proc/loadavg)
    out=$(env "$@" timeout 900 "$oracle" "$arg" 2>&1 | tail -3 | tr '\n' ' ')
    echo "[$label] rep$i load=$ld :: $out"
  done
}

echo
echo "############ ITEM 2 - ZipContentTests (OOM under JIT at 2g) ############"
run3 "zip/baseline-jit"      /tmp/ziporacle.sh ZipContentTests
run3 "zip/nojit"             /tmp/ziporacle.sh ZipContentTests CRATONVM_EXTRA_ARGS=--nojit
# Discriminator for the mistyped-static family: forces the helper, which reads
# the Value and widens, instead of the descriptor-width inline load.
run3 "zip/getstatic-helper"  /tmp/ziporacle.sh ZipContentTests CRATONVM_JIT=getstatic-helper
run3 "zip/no-statics-index"  /tmp/ziporacle.sh ZipContentTests CRATONVM_JIT=-statics-index
run3 "zip/no-osr"            /tmp/ziporacle.sh ZipContentTests CRATONVM_JIT=-osr

# Allocates more, or retains more? Same heap as every row above - this is
# instrumentation, not a heap-size sweep. If the JIT arm's LIVE set after a
# full GC matches the nojit arm's, the extra footprint is garbage the collector
# is not getting to; if the live set is genuinely bigger, something is pinning.
echo "--- zip GC instrumentation (1 run per arm, --Xmx 2g both) ---"
for arm in "jit:" "nojit:--nojit"; do
  name="${arm%%:*}"; extra="${arm#*:}"
  echo "### gc-overhead $name ###"
  env CRATONVM_DBG=gc-overhead CRATONVM_EXTRA_ARGS="$extra" timeout 900 \
      /tmp/ziporacle.sh ZipContentTests 2>&1 | grep -iE "gc-overhead|young_largest_free|OutOfMemory|SBRUNNER_RESULT|PASS|FAIL" | tail -25
done

echo
echo "############ ITEM 1 - OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb ############"
run3 "yaml/baseline-jit"     /tmp/yamloracle.sh x
run3 "yaml/nojit"            /tmp/yamloracle.sh x CRATONVM_EXTRA_ARGS=--nojit
run3 "yaml/no-osr"           /tmp/yamloracle.sh x CRATONVM_JIT=-osr
# NEW lever, never tried on this failure: the trampoline's frame-slot-store
# elision, added as a diagnosis lever by the Arrays.sort OSR investigation.
run3 "yaml/seed-frame-slots" /tmp/yamloracle.sh x CRATONVM_JIT_OSR_SEED_FRAME_SLOTS=1
run3 "yaml/dead-locals-off"  /tmp/yamloracle.sh x CRATONVM_JIT_OSR_DEAD_LOCALS=0
run3 "yaml/no-kernel-reg-osr"    /tmp/yamloracle.sh x CRATONVM_JIT=-kernel-reg-osr
run3 "yaml/no-kernel-reg-locals" /tmp/yamloracle.sh x CRATONVM_JIT=-kernel-reg-locals
run3 "yaml/getstatic-helper" /tmp/yamloracle.sh x CRATONVM_JIT=getstatic-helper
run3 "yaml/osr-single-pc"    /tmp/yamloracle.sh x CRATONVM_JIT_OSR_SINGLE_PC=1

echo
echo "=== DONE ==="; date -u
