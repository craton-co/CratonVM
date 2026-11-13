// JIT round j27 wave 6, lane T1OSRREENTRY: book
// docs/feature-designs/jit-osr-exit-and-recompile.md (OSRREENTRY21-1 / -3).
// The N arm's switch was deleted when the book retired (21-1 never engaged on this probe).
//
// Rows (each its own method, so no row runs a body another row compiled):
//   osr-every-call-act   spinA(5000): every activation's loop passes the 1000-back-edge OSR
//                        threshold, so after the first OSR compile every call enters the OSR body;
//                        the method-entry body comes from the loop-work credit (or, under
//                        CRATONVM_JIT_OSR_ENTRY_NOMINATES=1, from the OSR entry itself).
//   trap-recompile-act   spinB(5000, mode): 200 warm calls with mode 0 (the Late branch never
//                        taken, Late not loaded), then 200 with mode 1: the first Late.mix call
//                        traps the method-entry body; its recompile is the nomination this book
//                        is about (count already past the threshold, body gone).
//   R21AcceptOsrEarly short-loop-40-act / medium-loop-600-act (tools/probes/jit/r21) are the
//   OSRREENTRY21-3 rows; run them beside this probe.
//
// Expected output on HotSpot 25 and in every arm (S = 5000*4999/2 = 12497500):
//   fact osr-every-call-sum 7498500000        (200 calls x 3S = 200 x 37492500)
//   fact trap-recompile-warm-sum 7498500000   (200 calls x 3S)
//   fact trap-recompile-sum 34993000000       (200 calls x 14S = 200 x 174965000; mix(i) = 11i)
// Timing (only with -Dj27.time=true): `time <row> <ns> ns` per activation over the last 100 calls.
//
// Arms (interleaved, 5 reps, --real-jdk):
//   A <bin>
//   N <bin> CRATONVM_JIT_OSR_ENTRY_NOMINATES=1
//   with CRATONVM_DBG_JITC=1 once per arm: grep 'osr entry nominates' (N only) and the
//   J27T1OsrreentryRows.spin[AB] put lines (when the method-entry body publishes, by call number).
public class J27T1OsrreentryRows {
    static volatile long sink;
    static final boolean TIME = Boolean.getBoolean("j27.time");

    static final class Late {
        static int salt = 11;
        static long mix(int i) { return (long) i * salt; }
    }

    static long spinA(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i * 3L;
        }
        return s;
    }

    static long spinB(int n, int mode) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i * 3L;
            if (mode != 0) s += Late.mix(i);
        }
        return s;
    }

    static void time(String row, long t0, int calls) {
        if (TIME) System.out.println("time " + row + " " + ((System.nanoTime() - t0) / calls) + " ns");
    }

    public static void main(String[] args) {
        long a = 0;
        long t0 = 0;
        for (int c = 0; c < 200; c++) {
            if (c == 100) t0 = System.nanoTime();
            a += spinA(5000);
        }
        time("osr-every-call-act", t0, 100);

        long warm = 0;
        for (int c = 0; c < 200; c++) warm += spinB(5000, 0);
        long late = 0;
        for (int c = 0; c < 200; c++) {
            if (c == 100) t0 = System.nanoTime();
            late += spinB(5000, 1);
        }
        time("trap-recompile-act", t0, 100);
        sink = a + warm + late;

        System.out.println("fact osr-every-call-sum " + a);
        System.out.println("fact trap-recompile-warm-sum " + warm);
        System.out.println("fact trap-recompile-sum " + late);
    }
}
