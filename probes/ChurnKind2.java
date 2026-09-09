/** Is the leaker `new Object()` specifically, or any field-less object?
 *
 *      cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx256m \
 *          -cp probes ChurnKind2 object 5 125000
 *      cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx256m \
 *          -cp probes ChurnKind2 empty  5 125000
 *
 *  `Empty` is identical to `java.lang.Object` in every way the collector cares
 *  about — no fields, no array, no element type — except that it has a class
 *  entry, so its `class_id` is not zero and its first header word is not zero
 *  either. That single bit was the discriminator for
 *  `h2-testvaluememory-system-gc-retained-every-empty-object-FIXED-20260908`:
 *  `java/lang/Object` is `ClassId(0)`, so before `GC_FLAG_HEADER` a fresh
 *  `new Object()` published sixteen zero bytes that the young non-moving sweep
 *  could not tell from a hole.
 *
 *  Measured 2026-09-08, `usedKb` by round:
 *
 *      new Object(), before   3844  6662  9478  12295  15111   (+2.8 MB/round)
 *      new Empty(),  before   3460  5508  6532   6532   7799   (plateau)
 *      either,       after    1341  1346  1346   1346   1346
 *
 *  Run it as a pair. A single arm cannot separate "this shape leaks" from
 *  "this workload allocates", which is what makes the control the whole
 *  measurement here. */
public class ChurnKind2 {
    static class Empty {
    }

    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }

    public static void main(String[] args) {
        String which = args[0];
        int rounds = Integer.parseInt(args[1]);
        int n = Integer.parseInt(args[2]);
        for (int k = 0; k < rounds; k++) {
            for (int i = 0; i < n; i++) {
                Object o = which.equals("object") ? new Object() : new Empty();
                if (o == null) {
                    System.out.print("");
                }
            }
            System.out.println(which + " round=" + k + " usedKb=" + usedKb());
        }
    }
}
