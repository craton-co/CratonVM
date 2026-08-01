/**
 * What does a static callee actually receive when its argument is produced by
 * another call / getstatic rather than by an iload?
 *
 * Everything here is constant, so every expectation is exact. Single-threaded;
 * the loop only exists to get the methods JIT-compiled.
 *
 * usage: ArgMarshalProbe [iterations]
 */
public class ArgMarshalProbe {

    static final int NEG = -536870912; // RUNNING
    static int negStatic = NEG;
    static long negLongStatic = -1234567890123L;

    private static int neg() {
        return negStatic;
    }

    private static long negLong() {
        return negLongStatic;
    }

    private static int arg0of2(int a, int b) {
        return a;
    }

    private static int arg1of2(int a, int b) {
        return b;
    }

    private static int arg0of1(int a) {
        return a;
    }

    private static int arg0of3(int a, int b, int c) {
        return a;
    }

    private static int arg2of3(int a, int b, int c) {
        return c;
    }

    private static long arg0of2L(long a, long b) {
        return a;
    }

    private static boolean ge(int c, int s) {
        return c >= s;
    }

    // ---- the shapes -------------------------------------------------------
    static int t1() { return arg0of2(neg(), 0); }                 // arg0 from a Java call
    static int t2() { return arg0of2(negStatic, 0); }             // arg0 from a getstatic
    static int t3() { int c = negStatic; return arg0of2(c, 0); }  // arg0 from an iload (control)
    static int t4() { return arg1of2(neg(), 7); }                 // does arg1 survive?
    static int t5() { return arg0of1(neg()); }                    // 1-arg callee
    static int t6() { return arg0of3(neg(), 1, 2); }              // 3-arg callee, want arg0
    static int t7() { return arg2of3(neg(), 1, 2); }              // 3-arg callee, want arg2
    static int t8() { return arg0of2(0, neg()) + arg1of2(0, neg()); } // arg1 from a call
    static long t9() { return arg0of2L(negLong(), 0L); }          // long args
    static boolean t10() { return ge(neg(), 0); }                 // the original predicate

    public static void main(String[] args) {
        long iters = args.length > 0 ? Long.parseLong(args[0]) : 3_000_000L;
        int[] bad = new int[12];
        int[] sawA = new int[12];
        long badL = 0;
        long saw9 = 0;
        for (long i = 0; i < iters; i++) {
            int v;
            v = t1();  if (v != NEG) { bad[1]++; sawA[1] = v; }
            v = t2();  if (v != NEG) { bad[2]++; sawA[2] = v; }
            v = t3();  if (v != NEG) { bad[3]++; sawA[3] = v; }
            v = t4();  if (v != 7)   { bad[4]++; sawA[4] = v; }
            v = t5();  if (v != NEG) { bad[5]++; sawA[5] = v; }
            v = t6();  if (v != NEG) { bad[6]++; sawA[6] = v; }
            v = t7();  if (v != 2)   { bad[7]++; sawA[7] = v; }
            v = t8();  if (v != NEG) { bad[8]++; sawA[8] = v; }
            long l = t9(); if (l != negLongStatic) { badL++; saw9 = l; }
            if (t10())  { bad[10]++; }
        }
        String[] d = { "", "t1  arg0of2(neg(),0)", "t2  arg0of2(negStatic,0)",
                "t3  arg0of2(local,0)  [control]", "t4  arg1of2(neg(),7) want 7",
                "t5  arg0of1(neg())", "t6  arg0of3(neg(),1,2)", "t7  arg2of3(neg(),1,2) want 2",
                "t8  arg1 from a call", "", "t10 ge(neg(),0) want false" };
        System.out.println("iters=" + iters + " NEG=" + NEG);
        for (int i = 1; i <= 10; i++) {
            if (d[i].isEmpty()) {
                continue;
            }
            System.out.println("  " + d[i] + "  bad=" + bad[i]
                    + (bad[i] > 0 && i != 10 ? "  lastSaw=" + sawA[i] + " (0x" + Integer.toHexString(sawA[i]) + ")" : ""));
        }
        System.out.println("  t9  arg0of2L(negLong(),0)  bad=" + badL
                + (badL > 0 ? "  lastSaw=" + saw9 : ""));
        long tot = badL;
        for (int i = 1; i <= 10; i++) {
            tot += bad[i];
        }
        System.exit(tot > 0 ? 3 : 0);
    }
}
