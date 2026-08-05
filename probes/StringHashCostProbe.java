import java.util.HashMap;
import java.util.Map;

/**
 * What is the `String.hashCode()` native actually worth, now that the bytecode
 * under it is correct?
 *
 * It was registered `NativeKind::Intrinsic` on 2026-08-05 for CORRECTNESS: the
 * real `String.hashCode()` bytecode reached
 * `ArraysSupport.vectorizedHashCode(.., T_CHAR)`, which read one byte per
 * char instead of pairing them, so every non-Latin-1 string hashed wrong. That
 * defect is fixed. The registration therefore has to argue for itself on
 * performance again — which is the argument it was originally written for
 * (~1950x on a 5M-call microbench), except that number was measured against a
 * NON-caching inline closure, not against the real bytecode, and the real
 * bytecode caches in `String.hash` exactly like the native does.
 *
 * So this measures the two things that can actually differ:
 *
 *   * COLD  — first hash of each distinct string, i.e. the fold itself. This
 *     is where a Rust loop can beat an interpreted one.
 *   * WARM  — repeated hashing of the same strings, i.e. the cache hit. Both
 *     sides cache in the same field, so this should be a wash; if it is not,
 *     the difference is dispatch overhead, and a native call pays the
 *     `safe_native_call` funnel that bytecode does not.
 *
 * Half the corpus is Latin-1 and half is UTF-16, reported separately, because
 * only the UTF-16 half changed and mixing them would hide it.
 *
 * Prints a checksum so a faster-and-wrong arm is visible as such.
 */
public class StringHashCostProbe {

    static String[] corpus(int n, boolean utf16) {
        String[] out = new String[n];
        for (int i = 0; i < n; i++) {
            StringBuilder sb = new StringBuilder();
            for (int k = 0; k < 40; k++) {
                sb.append((char) ((utf16 ? 0x0390 : 'a') + ((i + k) % 26)));
            }
            sb.append(i);
            out[i] = sb.toString();
        }
        return out;
    }

    static long cold(String[] c, int reps) {
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            // A fresh copy per rep, so `hash` is unset and the FOLD runs.
            for (String s : c) {
                sum += new String(s.toCharArray()).hashCode();
            }
        }
        return sum;
    }

    static long warm(String[] c, int reps) {
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            for (String s : c) {
                sum += s.hashCode();
            }
        }
        return sum;
    }

    static long maps(String[] c, int reps) {
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            Map<String, Integer> m = new HashMap<>();
            for (int i = 0; i < c.length; i++) {
                m.put(c[i], i);
            }
            for (String s : c) {
                sum += m.get(s);
            }
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        String[] latin1 = corpus(n, false);
        String[] utf16 = corpus(n, true);

        long t0 = System.nanoTime();
        long a = cold(latin1, reps);
        long t1 = System.nanoTime();
        long b = cold(utf16, reps);
        long t2 = System.nanoTime();
        long c = warm(latin1, reps);
        long t3 = System.nanoTime();
        long d = warm(utf16, reps);
        long t4 = System.nanoTime();
        long e = maps(utf16, reps);
        long t5 = System.nanoTime();

        System.out.println("HASHCOST n=" + n + " reps=" + reps
                + " coldLatin1Ms=" + ((t1 - t0) / 1_000_000)
                + " coldUtf16Ms=" + ((t2 - t1) / 1_000_000)
                + " warmLatin1Ms=" + ((t3 - t2) / 1_000_000)
                + " warmUtf16Ms=" + ((t4 - t3) / 1_000_000)
                + " mapUtf16Ms=" + ((t5 - t4) / 1_000_000));
        System.out.println("HASHDIGEST " + a + " " + b + " " + c + " " + d + " " + e);
    }
}
