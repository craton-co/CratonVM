/** Does `Runtime.totalMemory() - freeMemory()` grow when a program retains objects?
 *  H2's `Utils.getMemoryUsed()` is exactly that expression, in KB, after a GC. */
public class HeapUsedDelta {
    static Object[] keep;
    static long usedKb() {
        System.gc();
        Runtime r = Runtime.getRuntime();
        return (r.totalMemory() - r.freeMemory()) >> 10;
    }
    public static void main(String[] a) {
        long before = usedKb();
        keep = new Object[400000];
        for (int i = 0; i < keep.length; i++) keep[i] = new long[8];
        long after = usedKb();
        System.out.println("before=" + before + "KB after=" + after
                + "KB delta=" + (after - before) + "KB retained=" + keep.length);
        System.out.println(after > before ? "GROWS" : "FLAT");
    }
}
