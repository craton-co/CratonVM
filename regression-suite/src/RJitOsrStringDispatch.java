/**
 * Regression: a `String.equals` dispatch chain gave WRONG ANSWERS, and read
 * past the end of a backing array, once the enclosing loop was OSR-promoted.
 *
 * History. `StackSlot::Scratch` gained a home offset on 2026-09-01 so
 * `flush_scratch_registers` could store into a word the push had already
 * reserved. The defect that was fixing is real, but the fix made
 * `push_from_rax` ADVANCE THE SPILL CURSOR, and the OSR entry's local homes
 * are derived from the same frame layout — so an OSR transition loaded the
 * wrong words. Reverted the same day in `c9b4a7d18`, which left the
 * frame-growth defect deliberately OPEN. The next attempt at it will touch the
 * same cursor, which is why this vector exists.
 *
 * The two symptoms, both with EQUAL-LENGTH operands:
 *
 *   1. `"f2i".equals("d2i")` returned true — 1565 times out of 4096, and
 *      interleaved rather than as a clean prefix.
 *   2. `ArrayIndexOutOfBoundsException: Index 3 out of bounds for length 3`
 *      thrown inside `String.equals` while comparing two 3-character strings.
 *
 * Two things about the shape are load-bearing and easy to lose in a rewrite:
 *
 *   * The taken arm must NOT be the first one. When the right answer is the
 *     first comparison, a wrongly-true first comparison is indistinguishable
 *     from correct behaviour — which is why the original repro looked clean
 *     for one of its three inputs and why a dispatch table that happens to
 *     test the taken arm first would not have caught this at all.
 *   * The counters compare by REFERENCE (`==` on the arm's own constant), not
 *     by `equals`. The subject must not also be the oracle: a broken `equals`
 *     would otherwise corrupt the count as well as the branch, and the
 *     original defect did exactly that — it reported `flips=0` while the
 *     counts showed both arms taken.
 *
 * `which` is a distinct object with equal contents, so `equals` cannot answer
 * from the identity fast path and has to run its comparison loop.
 *
 * Output is deterministic and diffed against HotSpot by run.sh.
 */
public class RJitOsrStringDispatch {

    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    static final String OTHER = "other";

    /**
     * counts[0..2] = arm 1..3 taken, counts[3] = fell through,
     * counts[4] = flips as reported by `last.equals(taken)`.
     */
    static int[] dispatch(String which, String k0, String k1, String k2, int n) {
        int[] counts = new int[5];
        String last = null;
        for (int i = 0; i < n; i++) {
            String taken;
            if (which.equals(k0)) {
                taken = k0;
            } else if (which.equals(k1)) {
                taken = k1;
            } else if (which.equals(k2)) {
                taken = k2;
            } else {
                taken = OTHER;
            }
            if (taken == k0) {
                counts[0]++;
            } else if (taken == k1) {
                counts[1]++;
            } else if (taken == k2) {
                counts[2]++;
            } else {
                counts[3]++;
            }
            if (last != null && !last.equals(taken)) {
                counts[4]++;
            }
            last = taken;
        }
        return counts;
    }

    static String key(char c, int len) {
        StringBuilder sb = new StringBuilder(len);
        for (int i = 0; i < len; i++) {
            sb.append(c);
        }
        return sb.toString();
    }

    /** An equal-valued string that is NOT the same object. */
    static String fresh(String s) {
        return new String(s.toCharArray());
    }

    static void one(String label, int arm, String which, String k0, String k1, String k2, int n) {
        int[] r = dispatch(which, k0, k1, k2, n);
        for (int a = 0; a < 4; a++) {
            int want = (a == arm) ? n : 0;
            check(r[a] == want,
                    label + ": arm " + a + " taken " + r[a] + " times, expected " + want);
        }
        check(r[4] == 0, label + ": last.equals(taken) reported " + r[4] + " flips in a loop that never changes arm");
    }

    public static void main(String[] args) {
        final int n = 20000;
        long matched = 0;
        long unmatched = 0;

        for (int len = 1; len <= 8; len++) {
            String k0 = key('a', len);
            String k1 = key('b', len);
            String k2 = key('c', len);

            // The SECOND arm: a wrongly-true first comparison is visible here
            // and nowhere else.
            one("len " + len + " arm2", 1, fresh(k1), k0, k1, k2, n);
            // The THIRD arm: two wrong comparisons have to stay wrong.
            one("len " + len + " arm3", 2, fresh(k2), k0, k1, k2, n);
            // Nothing matches, and every operand is the same length — the
            // shape that read index `len` of a length-`len` array.
            one("len " + len + " none", 3, key('z', len), k0, k1, k2, n);
            // The FIRST arm, which cannot fail on its own but pins the
            // control: if this one breaks, the chain is not the subject.
            one("len " + len + " arm1", 0, fresh(k0), k0, k1, k2, n);

            matched += 3L * n;
            unmatched += n;
        }

        check(matched == 8L * 3 * n, "matched total");
        check(unmatched == 8L * n, "unmatched total");

        System.out.println("CK RJitOsrStringDispatch matched=" + matched + " unmatched=" + unmatched);
        System.out.println("CK RJitOsrStringDispatch checks=" + checks);
        System.out.println("PASS RJitOsrStringDispatch (" + checks + " checks)");
    }
}
