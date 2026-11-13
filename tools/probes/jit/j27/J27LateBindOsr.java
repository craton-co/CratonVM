/**
 * C2W2-6: an OSR loop that calls a static of a class initialized AFTER the loop compiled. The answer must
 * be HotSpot's in every arm; with {@code CRATONVM_DBG_JITC=1} the log must show the late-bind recompile and
 * a direct bind afterwards ({@code grep -c 'DeclaringClassNotInitialized'}), and
 * {@code CRATONVM_JIT_LATE_BIND_OSR_OPTIMIZING=0} must give the same facts.
 * Expected HotSpot 25 lines: {@code fact cold 499999500000}, {@code fact late 500000500000}, {@code fact again 500000500000}.
 */
public class J27LateBindOsr {
    static class Late {
        static final int V;
        static {
            V = 1;
        }

        static long get(int i) {
            return i + V;
        }
    }

    static volatile boolean go;

    static long loop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            if (go) s += Late.get(i) - 1;
            else s += i;
        }
        return s + (go ? n : 0);
    }

    public static void main(String[] a) {
        go = false;
        System.out.println("fact cold " + loop(1_000_000));
        go = true; // Late is initialized from inside the next (already compiled) loop
        System.out.println("fact late " + loop(1_000_000));
        System.out.println("fact again " + loop(1_000_000));
    }
}
