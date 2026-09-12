// The same three-armed clamp as Min1, written STRAIGHT INTO the loop body:
// no call, nothing spliced, no combined-buffer pc anywhere in the graph.
//
// This is the reproducer that corrected the page's localisation. The defect
// is not "a merge inside a relocated callee" — it is any in-loop merge one of
// whose arms is a LITERAL, because a merge store reads its arms from their
// home words and the OSR entry stub jumps past the block that writes them.
//
//   expected (HotSpot, `java -cp . Min0 8 800000`)   clamp=5134452788
//   before the fix                                    a different wrong sum
//                                                     on every run
//
// `CRATONVM_JIT_IR_OSR_ENTRY=0` is the localising arm: correct with the entry
// stub withdrawn, wrong with it — on a program that splices nothing.
public class Min0 {
    static long runClamp(int n, int seed) {
        long s = 0;
        int x = seed;
        for (int i = 0; i < n; i++) {
            x = x * 1103515245 + 12345;
            int v = x >>> 20;
            int r;
            if (v < 100) r = 100; else if (v > 900) r = 900; else r = v;
            s += r;
        }
        return s;
    }

    public static void main(String[] a) {
        int reps = Integer.parseInt(a[0]);
        int n = Integer.parseInt(a[1]);
        long acc = 0;
        for (int i = 0; i < reps; i++) acc += runClamp(n, 12345 + i);
        System.out.println("clamp=" + acc);
    }
}
