/**
 * What does ONE caught exception cost inside an OSR'd loop?
 *
 * The RBC.6b lift (2026-08-17) made a once-invoked `try`/`catch` loop compile,
 * and the `[cratonvm] OSR lifecycle:` line then showed what that costs: on
 * `HeaderValidationLoopRate` at n=1e6, `osr_entered=1080047` beside
 * `osr_exception_handler_entered=1080038` — i.e. every caught exception is a
 * full OSR exit followed by a re-entry at the next back edge, because the
 * compiled body has no way to enter its own handler.
 *
 * This measures the slope of that. Every arm is the SAME method shape, reached
 * exactly ONCE, differing only in how often the callee throws:
 *
 *   rate=0     no throws at all — the control, and the compiled floor
 *   rate=1024  one throw in 1024
 *   rate=64    one throw in 64
 *   rate=8     one throw in 8    (near the netty value loop's 7.7%)
 *   rate=1     every iteration
 *
 * `(t(rate) - t(0)) * rate` is the per-throw cost. A probe that reported only
 * the throw-all number could not tell an expensive throw from a slow loop; the
 * control is what separates them, and it must have the same loop shape or an
 * OSR artifact difference eats the result.
 *
 * Run it against HotSpot too. The gap there is the whole point: HotSpot
 * compiles the handler INTO the body and never leaves compiled code.
 *
 *   javac -nowarn -d . probes/OsrExcRateProbe.java
 *   java                          -cp . OsrExcRateProbe 4000000
 *   cratonvm --java-home <jdk>    -cp . OsrExcRateProbe 4000000
 *   CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp . OsrExcRateProbe 4000000
 */
public final class OsrExcRateProbe {

    static final RuntimeException E = new RuntimeException() {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static long sink;
    static long caught;

    static void mayThrow(int i, int mask) {
        if (mask != 0 && (i & mask) == 0) {
            throw E;
        }
    }

    // Five identical bodies, because each must be reached exactly ONCE for OSR
    // to be its only door — the shape a `@Test` method has. One method called
    // five times would be method-entry compiled after the first, which is a
    // different tier and a different question.
    static void arm0(int n) { int i = 0; do { long a = 0; try { mayThrow(i, 0);    } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }
    static void arm1k(int n){ int i = 0; do { long a = 0; try { mayThrow(i, 1023); } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }
    static void arm64(int n){ int i = 0; do { long a = 0; try { mayThrow(i, 63);   } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }
    static void arm8(int n) { int i = 0; do { long a = 0; try { mayThrow(i, 7);    } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }
    static void arm1(int n) { int i = 0; do { long a = 0; try { mayThrow(i, 0);    } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }

    // arm1 must actually throw every time; `mask == 0` is the no-throw sentinel,
    // so it gets its own thrower rather than a mask.
    static void alwaysThrow(int i) { throw E; }
    static void armAll(int n) { int i = 0; do { long a = 0; try { alwaysThrow(i); } catch (RuntimeException e) { caught++; a = i; } sink += a; i++; } while (i != n); }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;

        long t0 = time(() -> arm0(n));
        report("rate=0    ", t0, n, 0);
        long t1k = time(() -> arm1k(n));
        report("rate=1/1024", t1k, n, 1024);
        long t64 = time(() -> arm64(n));
        report("rate=1/64 ", t64, n, 64);
        long t8 = time(() -> arm8(n));
        report("rate=1/8  ", t8, n, 8);
        long tAll = time(() -> armAll(n));
        report("rate=1/1  ", tAll, n, 1);

        System.out.println("--- per-throw cost, against the rate=0 control ---");
        perThrow("1/1024", t1k, t0, n, 1024);
        perThrow("1/64  ", t64, t0, n, 64);
        perThrow("1/8   ", t8, t0, n, 8);
        perThrow("1/1   ", tAll, t0, n, 1);
        System.out.println("sink=" + sink + " caught=" + caught);
        // arm1 is unused (armAll replaced it); keep it referenced so a future
        // edit cannot orphan it silently.
        if (args.length > 99) { arm1(1); }
    }

    static long time(Runnable r) {
        long a = System.nanoTime();
        r.run();
        return System.nanoTime() - a;
    }

    static void report(String name, long ns, int n, int rate) {
        System.out.printf("%-11s %10.2f ns/iter%n", name, (double) ns / n);
    }

    static void perThrow(String name, long ns, long base, int n, int rate) {
        double throwsPerIter = 1.0 / rate;
        double delta = (double) (ns - base) / n;
        System.out.printf("  %s  %+10.2f ns/iter over control  =>  %10.1f ns per throw%n",
                name, delta, delta / throwsPerIter);
    }
}
