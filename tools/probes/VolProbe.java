/** Isolates the cost of a VOLATILE field read, which is what
 *  ZgcRealHeap::get_field_volatile serves. `plain` is the control: the same
 *  loop over a non-volatile field, so the difference between the two rows is
 *  the volatile machinery and nothing else. */
public class VolProbe {
    static class Holder {
        volatile int vi;
        volatile Object vo;
        int pi;
        Object po;
    }

    static final Holder H = new Holder();

    static long vint(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += H.vi;
        return s;
    }

    static long pint(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += H.pi;
        return s;
    }

    static long vref(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) if (H.vo != null) s++;
        return s;
    }

    static long pref(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) if (H.po != null) s++;
        return s;
    }

    /** Four threads reading the SAME volatile field: the arm where a striped
     *  process-global mutex behind the read turns into real contention. */
    static long vshared(int n) throws Exception {
        final int threads = 4;
        Thread[] ts = new Thread[threads];
        final long[] out = new long[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long s = 0;
                for (int i = 0; i < n; i++) s += H.vi;
                out[id] = s;
            });
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long s = 0;
        for (long v : out) s += v;
        return s;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "vint";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20_000_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        H.vi = 7;
        H.pi = 7;
        H.vo = H;
        H.po = H;
        long chk = 0;
        for (int r = 0; r <= rounds; r++) {
            long t0 = System.nanoTime();
            switch (mode) {
                case "vint": chk = vint(n); break;
                case "pint": chk = pint(n); break;
                case "vref": chk = vref(n); break;
                case "pref": chk = pref(n); break;
                case "vshared": chk = vshared(n); break;
                default: throw new IllegalArgumentException(mode);
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("r" + r + " " + mode + " " + ms + " ms chk=" + chk);
        }
    }
}
