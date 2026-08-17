import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Function;

/**
 * Which primitive inside `CompletableFuture` composition is the amplifier?
 *
 * Written 2026-08-17. `CompletionStageChainProbe` measured one composition step
 * at 16–46x HotSpot `-Xint`, against ~2.2x for plain boxing — i.e. an order of
 * magnitude worse than this VM's own documented per-call baseline, and with the
 * JIT contributing nothing (a `--nojit` arm is the same speed or faster).
 * `CompletableFuture` completion is built out of exactly three things a
 * composition step touches that a plain allocation does not, so measure them
 * separately against the same `-Xint` control:
 *
 *   cas       — `AtomicReference.compareAndSet`, standing in for CF's
 *               `UNSAFE.compareAndSetReference(this, RESULT, null, r)`, which
 *               every completion and every stack push performs
 *   lambda    — an indirect call through a `Function` field, which is what
 *               every `thenApply`/`thenCompose` body is
 *   volatileRW— a volatile reference read+write, CF's `result`/`stack` access
 *   iface     — the same indirect call through an interface the JIT cannot
 *               monomorphise, for contrast with the single-target `lambda`
 *
 * Read each against the `-Xint` column, not against C2: the project's own
 * yardstick is "2.5x versus -Xint is the statement about this VM".
 */
public class CompositionPrimitivesProbe {

    private static final int WARMUP = 50_000;
    private static final int ITERS = 500_000;

    private static Object sink;
    private static volatile Object vsink;

    private static final AtomicReference<Object> REF = new AtomicReference<>(null);
    private static final Function<Integer, Integer> FN = v -> v + 1;
    private static final Function<Integer, Integer> FN2 = v -> v + 2;

    private static long cas(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Object cur = REF.get();
            REF.compareAndSet(cur, cur == null ? FN : null);
        }
        return System.nanoTime() - t0;
    }

    private static long lambda(int n) {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += FN.apply(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long iface(int n) {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            Function<Integer, Integer> f = (i & 1) == 0 ? FN : FN2;
            acc += f.apply(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long volatileRW(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Object cur = vsink;
            vsink = cur == null ? FN : null;
        }
        return System.nanoTime() - t0;
    }

    private static long plainCall(int n) {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += add1(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static int add1(int v) {
        return v + 1;
    }

    private static void report(String name, long nanos, int n) {
        System.out.printf("%-11s %9.1f ns/op  (%d ops)%n", name, (double) nanos / n, n);
    }

    public static void main(String[] args) {
        plainCall(WARMUP);
        lambda(WARMUP);
        iface(WARMUP);
        cas(WARMUP);
        volatileRW(WARMUP);

        report("plainCall", plainCall(ITERS), ITERS);
        report("lambda", lambda(ITERS), ITERS);
        report("iface", iface(ITERS), ITERS);
        report("volatileRW", volatileRW(ITERS), ITERS);
        report("cas", cas(ITERS), ITERS);
        System.out.println("PROBE-DONE sink=" + (sink != null) + " ref=" + (REF.get() != null));
    }
}
