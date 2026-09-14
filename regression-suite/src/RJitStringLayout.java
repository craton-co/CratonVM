/**
 * Regression: two JIT codegen defects found behind the H2 `org/h2/` JIT ban
 * (`docs/known-issues/h2/h2-jitban-schema-not-found-on-reconnect.md`). Both
 * are general x64-backend bugs — H2 was only the messenger.
 *
 * 1. BUG-STRING-CODER-COMPACT: the inlined `java/lang/String` intrinsics read
 *    `coder` and `hash` 4 bytes past their real address in a COMPACT-laid-out
 *    instance, so `length()` evaluated `value.length >> (hash & 31)` and
 *    `hashCode()` returned the `hashIsZero` flag. Invisible until a String's
 *    lazily-cached `hash` is populated — hence the HashMap warm-up below.
 *
 * 2. BUG-JOIN-MIRROR: the reload-elision mirror recorded by a merge-point
 *    stack canonicalization survived the control-flow join, so
 *    `return c ? a : b` returned a stale register along the branch edge.
 *
 * Both need the methods to be JIT-compiled, so every helper is called in a
 * hot loop. Output is deterministic and diffed against HotSpot by run.sh.
 */
public class RJitStringLayout {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    // --- 1. String intrinsics on hash-cached receivers -----------------------
    // Small, separately-compiled callers: the intrinsics are inlined at the
    // CALL SITE, so they must sit in their own JIT-compiled method.
    static int len(String s)                 { return s.length(); }
    static boolean emp(String s)             { return s.isEmpty(); }
    static int hc(String s)                  { return s.hashCode(); }
    static char ch(String s, int i)          { return s.charAt(i); }
    static int idx(String s, String n)       { return s.indexOf(n); }
    static boolean eq(String s, String t)    { return s.equals(t); }
    static int cmp(String s, String t)       { return s.compareTo(t); }

    // --- 2. ternary return across a branch join -----------------------------
    static String orDefault(String s, String defaultValue) {
        return s == null ? defaultValue : s;
    }

    static int hashRef(String s) {
        int h = 0;
        for (char c : s.toCharArray()) h = 31 * h + c;
        return h;
    }

    static int idxRef(String s, char c) {
        char[] v = s.toCharArray();
        for (int i = 0; i < v.length; i++) if (v[i] == c) return i;
        return -1;
    }

    static int cmpRef(String s, String t) {
        char[] x = s.toCharArray(), y = t.toCharArray();
        int n = Math.min(x.length, y.length);
        for (int i = 0; i < n; i++) if (x[i] != y[i]) return x[i] - y[i];
        return x.length - y.length;
    }

    public static void main(String[] args) {
        // "PUBLIC" is the exact string H2 tripped over: its hashCode() is
        // -1924094359, whose low 5 bits are 9, so the buggy `length()` shifted
        // 6 right by 9 and answered 0 — H2 then persisted `CREATE SEQUENCE
        // ""."SEQ1"` and could never reopen the database.
        String[] pool = { "PUBLIC", "", "x", "hello world", "SEQ1", "AbCdEfGh", "café", "A中Z" };
        int[] refLen = new int[pool.length];
        int[] refHash = new int[pool.length];
        for (int i = 0; i < pool.length; i++) {
            refLen[i] = pool[i].toCharArray().length;
            refHash[i] = hashRef(pool[i]);
        }
        // Populate every String's lazy `hash` cache — without this the buggy
        // read of `hash`-as-`coder` returns 0 and looks correct.
        java.util.HashMap<String, Integer> warm = new java.util.HashMap<>();
        for (int i = 0; i < pool.length; i++) warm.put(pool[i], i);

        int bad = 0;
        long sink = 0;
        for (int i = 0; i < 400_000; i++) {
            int k = i % pool.length;
            String s = pool[k];
            if (len(s) != refLen[k]) bad++;
            if (emp(s) != (refLen[k] == 0)) bad++;
            if (hc(s) != refHash[k]) bad++;
            if (refLen[k] > 0 && ch(s, 0) != pool[k].toCharArray()[0]) bad++;
            if (idx(s, "l") != idxRef(s, 'l')) bad++;
            if (!eq(s, new String(pool[k].toCharArray()))) bad++;
            if (Integer.signum(cmp(s, "PUBLIC")) != Integer.signum(cmpRef(pool[k], "PUBLIC"))) bad++;

            // Alternate the two edges of the ternary so the join is exercised
            // in both directions after the method is compiled.
            String d = orDefault((i & 1) == 0 ? null : s, "rw");
            if ((i & 1) == 0) { if (!"rw".equals(d)) bad++; } else if (d != s) bad++;
            sink += d.length();
        }
        check(bad == 0, "JIT String-intrinsic / ternary-join mismatches: " + bad);
        check(sink > 0, "sink");

        // A direct spot-check that survives even if the loop above is never
        // compiled, so the class still asserts something meaningful.
        check("PUBLIC".length() == 6, "PUBLIC length");
        check("PUBLIC".hashCode() == -1924094359, "PUBLIC hashCode");
        check("".hashCode() == 0, "empty hashCode");
        check(orDefault(null, "rw").equals("rw"), "orDefault null");
        check(orDefault("x", "rw").equals("x"), "orDefault non-null");

        System.out.println("CK RJitStringLayout sink=" + sink);
        System.out.println("PASS RJitStringLayout (" + checks + " checks)");
    }
}
