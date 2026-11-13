/**
 * IRLOWER17-3 / IRLOWER17-4 repro: a recursive in-place quicksort (the shape of Spring Boot's
 * {@code ZipContent$Loader.sort}: instance method, field arrays, a spliced {@code swap}, two self calls)
 * under the forced-chain soak. {@code CRATONVM_DEOPT_EAGER_CHAINS=1} must give HotSpot's lines.
 * Expected: {@code fact sorted true} and the same {@code fact checksum} on every run and in every arm.
 */
public class J27SortChainsRows {
    int[] keys;
    int[] vals;

    J27SortChainsRows(int n, int seed) {
        keys = new int[n];
        vals = new int[n];
        int x = seed;
        for (int i = 0; i < n; i++) {
            x = x * 1103515245 + 12345;
            keys[i] = x;
            vals[i] = i;
        }
    }

    void sort(int left, int right) {
        if (left < right) {
            int pivot = this.keys[left + (right - left) / 2];
            int i = left;
            int j = right;
            while (i <= j) {
                while (this.keys[i] < pivot) i++;
                while (this.keys[j] > pivot) j--;
                if (i <= j) {
                    swap(i, j);
                    i++;
                    j--;
                }
            }
            if (left < j) sort(left, j);
            if (right > i) sort(i, right);
        }
    }

    void swap(int i, int j) {
        int k = keys[i];
        keys[i] = keys[j];
        keys[j] = k;
        int v = vals[i];
        vals[i] = vals[j];
        vals[j] = v;
    }

    public static void main(String[] a) {
        long sum = 0;
        boolean sorted = true;
        for (int round = 0; round < 3000; round++) {
            J27SortChainsRows s = new J27SortChainsRows(400 + (round % 50) * 13, round);
            s.sort(0, s.keys.length - 1);
            for (int i = 1; i < s.keys.length; i++) sorted &= s.keys[i - 1] <= s.keys[i];
            for (int i = 0; i < s.keys.length; i += 7) sum += (long) s.keys[i] * 31 + s.vals[i];
        }
        System.out.println("fact sorted " + sorted);
        System.out.println("fact checksum " + sum);
    }
}
