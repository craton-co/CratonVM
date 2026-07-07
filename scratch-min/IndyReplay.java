/**
 * Repro for the reason-8 (invokedynamic uncommon trap) side-effect corruption
 * (docs/known-issues/hib-temporal-sql-parameter-placeholder-duplication.md /
 * jit-invokedynamic-uncommon-trap-precise-resume-groovy-regression.md).
 *
 * Three shapes, each with a caller-visible side effect committed BEFORE a live
 * invokedynamic (string concat):
 *
 *   1. direct:  interpreter -> compiled leaf with append-then-indy.
 *   2. nested:  interpreter -> compiled middle -> compiled leaf (the sentinel
 *               must be resolved at the leaf's own dispatch site, not by
 *               re-running the middle).
 *   3. osr:     a hot loop in ONE method appending then hitting the indy each
 *               iteration (OSR-frame shape).
 *
 * With the imprecise safe-reject each shape duplicates (or drops) appends once
 * the JIT kicks in; with the precise resume every count is exact.
 */
public class IndyReplay {
    static String tag(StringBuilder sb, int pos) {
        sb.append('?');           // side effect BEFORE the indy
        return "m" + pos;         // live invokedynamic (string concat)
    }

    static String middle(StringBuilder sb, int pos) {
        sb.append('#');           // middle's own side effect before calling leaf
        return tag(sb, pos);      // nested compiled call
    }

    public static void main(String[] args) {
        int bad = 0;
        // Shape 1: direct
        for (int round = 0; round < 30000; round++) {
            StringBuilder sb = new StringBuilder();
            String r = tag(sb, round);
            if (sb.length() != 1 || !r.equals("m" + round)) {
                if (++bad <= 5) {
                    System.out.println("BAD direct round=" + round + " len=" + sb.length()
                            + " sb=[" + sb + "] r=[" + r + "]");
                }
            }
        }
        System.out.println("direct bad=" + bad);
        int bad2 = 0;
        // Shape 2: nested
        for (int round = 0; round < 30000; round++) {
            StringBuilder sb = new StringBuilder();
            String r = middle(sb, round);
            if (sb.length() != 2 || sb.charAt(0) != '#' || sb.charAt(1) != '?'
                    || !r.equals("m" + round)) {
                if (++bad2 <= 5) {
                    System.out.println("BAD nested round=" + round + " len=" + sb.length()
                            + " sb=[" + sb + "] r=[" + r + "]");
                }
            }
        }
        System.out.println("nested bad=" + bad2);
        int bad3 = 0;
        // Shape 3: OSR — one hot loop, append + indy per iteration
        {
            StringBuilder sb = new StringBuilder();
            String last = "";
            for (int i = 0; i < 30000; i++) {
                sb.append('?');
                last = "x" + i;
            }
            if (sb.length() != 30000 || !last.equals("x29999")) {
                bad3 = 1;
                System.out.println("BAD osr len=" + sb.length() + " last=[" + last + "]");
            }
            System.out.println("osr bad=" + bad3);
        }
        System.out.println((bad + bad2 + bad3) == 0 ? "@@PASS" : "@@FAIL");
        System.out.println("@@DONE");
    }
}
