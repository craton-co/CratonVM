import java.util.Arrays;
import java.util.Comparator;

/**
 * SBR4 OSR: TimSort.binarySort entered/exited through OSR with a low back-edge threshold
 * (CRATONVM_TIER_OSR_BACKEDGE=10, optionally CRATONVM_BG_COMPILE=0). Prints the first failing
 * round's input and output so the mis-sorted insertion is visible; HotSpot prints only the hash.
 */
public class Sbr4OsrSort {
    public static void main(String[] args) {
        Comparator<Integer> cmp = (x, y) -> Integer.compare(x, y);
        long h = 0;
        long seed = 12345;
        int rounds = Integer.getInteger("rounds", 4000);
        for (int r = 0; r < rounds; r++) {
            int n = 3 + (r % 9);
            Integer[] a = new Integer[n];
            for (int i = 0; i < n; i++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                a[i] = (int) (seed >>> 40) & 0xff;
            }
            Integer[] in = a.clone();
            Arrays.sort(a, cmp);
            for (int i = 1; i < n; i++) {
                if (a[i - 1] > a[i]) {
                    System.out.println("BAD round " + r + " in=" + Arrays.toString(in) + " out=" + Arrays.toString(a));
                    throw new AssertionError("unsorted at round " + r);
                }
            }
            for (int v : a) h = h * 31 + v;
        }
        System.out.println("sorted-hash=" + h);
    }
}
