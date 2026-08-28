import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReferenceArray;

/**
 * Prices the rungs the CURRENT `--dump-native-registry` census puts on
 * `HashedWheelTimerTest#testExecutionOnTime`'s hot path.
 *
 * `TimerNativeRungRate` prices the 2026-08-19 census, and that census is
 * stale: `setExclusiveOwnerThread` (424,196 then) is 3,985 now, `AtomicInteger
 * .get` (304,967) is 1,053, and every `VarHandle` row and both `Long` box rows
 * are gone. Pricing rungs that no longer fire is how a page keeps recommending
 * a fix for a cost somebody already removed, so this is a second probe rather
 * than an edit to that one — both censuses stay readable.
 *
 * Re-taken 2026-08-27, dev `8f8730a24`, `HwtScaleProbe 100000`, 1,035,763
 * registered-native invocations for 100,000 expired tasks (10.4 per task):
 *
 *   300 023  3.00/task  System.nanoTime()J                              bridge
 *   200 288  2.00/task  AtomicReferenceArray.lazySet(ILjava/lang/Object;)V  intrinsic
 *   152 844  1.53/task  Thread.interrupted()Z                           bridge
 *   100 218  1.00/task  AtomicReferenceArray.get(I)Ljava/lang/Object;   intrinsic
 *   100 002  1.00/task  TimeUnit.toNanos(J)J                            bridge
 *   100 000  1.00/task  TimeUnit.toMillis(J)J                           bridge
 *    27 364  0.27/task  Thread.isInterrupted()Z                         bridge
 *
 * The `AtomicReferenceArray` pair is netty's MPSC timeout queue (the
 * `AtomicReferenceArray`-backed jctools variant): `offer` is one `soElement`,
 * `poll` is one `lvElement` plus one `soElement(null)`, which is exactly the
 * 2 + 1 the census shows.
 *
 * Every arm is its own small method called REPS times — never a loop inline in
 * `main`, which on this VM measures the interpreter rather than compiled code.
 * Run the identical class on HotSpot for the control.
 *
 *   javac -d out probes/FunnelRungRate.java
 *   java      -cp out FunnelRungRate 4000000 40          # control
 *   cratonvm --java-home <jdk> -cp out FunnelRungRate 4000000 40
 */
public final class FunnelRungRate {

    static final AtomicReferenceArray<Object> ARA = new AtomicReferenceArray<Object>(64);
    static final Object VALUE = new Object();
    static long sink;
    static Object osink;

    static long a_empty(int n)       { long a = 0; for (int i = 0; i < n; i++) { a += i; } return a; }
    static long b_nanotime(int n)    { long a = 0; for (int i = 0; i < n; i++) { a += System.nanoTime(); } return a; }
    static long c_ara_set(int n)     { for (int i = 0; i < n; i++) { ARA.lazySet(i & 63, VALUE); } return n; }
    static long d_ara_get(int n)     { Object o = null; for (int i = 0; i < n; i++) { o = ARA.get(i & 63); } osink = o; return n; }
    static long e_interrupted(int n) { long a = 0; for (int i = 0; i < n; i++) { if (Thread.interrupted()) { a++; } a += i; } return a; }
    static long f_tounit(int n)      { long a = 0; for (int i = 0; i < n; i++) { a += TimeUnit.NANOSECONDS.toMillis(i) + TimeUnit.MILLISECONDS.toNanos(i & 0xFF); } return a; }
    static long g_isinterrupted(int n) { long a = 0; Thread t = Thread.currentThread(); for (int i = 0; i < n; i++) { if (t.isInterrupted()) { a++; } a += i; } return a; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int r = 0; r < 3; r++) { sink += arm.run(per); }
        long best = Long.MAX_VALUE;
        long total = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            sink += arm.run(per);
            long dt = System.nanoTime() - t0;
            if (dt < best) { best = dt; }
            total += dt;
        }
        System.out.printf("%-28s best=%9.2f ns/op  mean=%9.2f ns/op%n",
                name, best / (double) per, (total / (double) reps) / (double) per);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("empty (baseline)", FunnelRungRate::a_empty, n, reps);
        time("System.nanoTime", FunnelRungRate::b_nanotime, n, reps);
        time("AtomicReferenceArray.lazySet", FunnelRungRate::c_ara_set, n, reps);
        time("AtomicReferenceArray.get", FunnelRungRate::d_ara_get, n, reps);
        time("Thread.interrupted", FunnelRungRate::e_interrupted, n, reps);
        time("TimeUnit.toMillis+toNanos", FunnelRungRate::f_tounit, n, reps);
        time("Thread.isInterrupted", FunnelRungRate::g_isinterrupted, n, reps);
        System.out.println("sink=" + (sink == 0 ? 1 : 0) + " osink=" + (osink == null ? 1 : 0));
    }
}
