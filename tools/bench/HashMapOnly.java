import java.util.HashMap;

/**
 * HashMapOnly — isolated HashMap put/get benchmark (README row).
 * value = 31*i + 7 (int), keys 0..n-1, checksum = sum of get() values.
 *   n=1,000,000  -> 15499991500000
 *   n=30,000,000 -> 13949999745000000
 * Kernel lives in a static method (NOT main): main contains an
 * invokedynamic string concat, which bails the whole-method OSR artifact
 * compile and silently OSR-denies it — the loop would run interpreted
 * forever (found 2026-07-17).
 */
public class HashMapOnly {
    static long run(int n) {
        HashMap<Integer, Integer> map = new HashMap<>();
        for (int i = 0; i < n; i++) {
            map.put(i, i * 31 + 7);
        }
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += map.get(i);
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        long t0 = System.currentTimeMillis();
        long sum = run(n);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("HashMap " + n + " put/get: " + elapsed + " ms  [" + sum + "]");
    }
}
