import java.util.Arrays;
import java.util.Comparator;

/**
 * SBR3 UNSAFECAS: TimSort.binarySort re-entered by OSR at a second loop header. Run with
 * CRATONVM_TIER_OSR_BACKEDGE=10 (default mode): RJdkJmx died 1 run in ~20 with SIGSEGV in the OSR body of
 * TimSort.binarySort entered at pc 69 (rsi=4 where a reference belongs).
 */
public class Sbr3UnsafecasSort {
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
            Arrays.sort(a, cmp);
            for (int i = 1; i < n; i++) if (a[i - 1] > a[i]) throw new AssertionError("unsorted at round " + r);
            for (int v : a) h = h * 31 + v;
        }
        System.out.println("sorted-hash=" + h);
    }
}
