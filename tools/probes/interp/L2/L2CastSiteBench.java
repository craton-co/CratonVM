// Interpreter round i1, lane L2 — cast-site / anewarray hot-path measurement.
//
// What to measure: the per-iteration cost of the four loops below on CratonVM
// with the JIT off (CRATONVM_DISABLE_JIT=1), before and after the lane-L2
// change, interleaving binaries and taking the median of the reps (in-JVM
// timings on this host swing ~3x between reps). Timings go to STDERR; STDOUT
// carries only checksums and must match HotSpot 25 exactly:
//   mono  checksum=4000000
//   poly  checksum=4000000
//   neg   checksum=0
//   anew  checksum=16000000
//
// Expected effect of the change:
//   mono  — `checkcast`/`instanceof` hit answered from the per-site receiver
//           memo: no class_manager read lock, no is_subclass_of walk.
//   poly  — two receiver classes alternate at one site: every hit re-walks and
//           re-memoises (a regression guard: must not get slower than before).
//   neg   — negative instanceof: unchanged path (full name-based fallbacks);
//           the baseline for the proposal page on a negative memo.
//   anew  — `anewarray` of a user class: component resolution answered from
//           the cast-site table instead of a String + full resolution per array.
// `CRATONVM_DBG_FIELD_SITE=1` prints `cast: hit/miss/fill/unusable` at exit —
// read it before quoting a number (a hit count of zero means the lever never
// fired).
public class L2CastSiteBench {
    interface Shape { int size(); }
    static final class Sq implements Shape { public int size() { return 1; } }
    static final class Ci implements Shape { public int size() { return 1; } }
    static final class Other {}

    static final int N = 4_000_000;

    static int mono(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i & 1];
            if (o instanceof Shape) s += ((Shape) o).size();
        }
        return s;
    }

    static int neg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (xs[i & 1] instanceof Shape) s++;
        }
        return s;
    }

    static int anew() {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Sq[] a = new Sq[4];
            s += a.length;
        }
        return s;
    }

    public static void main(String[] args) {
        Object[] monoXs = { new Sq(), new Sq() };
        Object[] polyXs = { new Sq(), new Ci() };
        Object[] negXs = { new Other(), new Other() };
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            int a = mono(monoXs);
            long t1 = System.nanoTime();
            int b = mono(polyXs);
            long t2 = System.nanoTime();
            int c = neg(negXs);
            long t3 = System.nanoTime();
            int d = anew();
            long t4 = System.nanoTime();
            if (rep == 2) {
                System.out.println("mono  checksum=" + a);
                System.out.println("poly  checksum=" + b);
                System.out.println("neg   checksum=" + c);
                System.out.println("anew  checksum=" + d);
            }
            System.err.printf("rep %d: mono %.1f ns/it  poly %.1f ns/it  neg %.1f ns/it  anew %.1f ns/it%n",
                rep,
                (t1 - t0) / (double) N, (t2 - t1) / (double) N,
                (t3 - t2) / (double) N, (t4 - t3) / (double) N);
        }
    }
}
