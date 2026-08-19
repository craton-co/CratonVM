import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Prices the rungs the `--dump-native-registry` census puts on
 * `HashedWheelTimerTest#testExecutionOnTime`'s hot path — 23 native calls per
 * expired task, of which these are the top rows:
 *
 *   424 196  AbstractOwnableSynchronizer.setExclusiveOwnerThread   (ReentrantLock)
 *   304 967  AtomicInteger.get
 *   300 017  System.nanoTime
 *   299 283  VarHandle.setRelease
 *   126 489  Thread.interrupted
 *   100 000  TimeUnit.toMillis / toNanos / Long.valueOf / Long.longValue
 *            AtomicLong.incrementAndGet / decrementAndGet
 *
 * Every arm is its own small method called REPS times — never a loop inline in
 * `main`, which on this VM measures the interpreter rather than compiled code.
 * Run the identical class on HotSpot for the control.
 *
 *   cratonvm --java-home <jdk> -cp <out> TimerNativeRungRate 4000000 40
 */
public final class TimerNativeRungRate {

    static final AtomicInteger AI = new AtomicInteger(7);
    static final AtomicLong AL = new AtomicLong(7);
    static final ReentrantLock LOCK = new ReentrantLock();
    static final LinkedBlockingQueue<Long> Q = new LinkedBlockingQueue<Long>();
    static long sink;

    static long a_empty(int n)      { long a = 0; for (int i = 0; i < n; i++) { a += i; } return a; }
    static long b_ai_get(int n)     { long a = 0; for (int i = 0; i < n; i++) { a += AI.get(); } return a; }
    static long c_nanotime(int n)   { long a = 0; for (int i = 0; i < n; i++) { a += System.nanoTime(); } return a; }
    static long d_al_incdec(int n)  { long a = 0; for (int i = 0; i < n; i++) { a += AL.incrementAndGet() + AL.decrementAndGet(); } return a; }
    static long e_box(int n)        { long a = 0; for (int i = 0; i < n; i++) { Long v = Long.valueOf(i); a += v.longValue(); } return a; }
    static long f_tounit(int n)     { long a = 0; for (int i = 0; i < n; i++) { a += TimeUnit.NANOSECONDS.toMillis(i) + TimeUnit.MILLISECONDS.toNanos(i & 0xFF); } return a; }
    static long g_lock(int n)       { long a = 0; for (int i = 0; i < n; i++) { LOCK.lock(); a += i; LOCK.unlock(); } return a; }
    static long h_interrupted(int n){ long a = 0; for (int i = 0; i < n; i++) { if (Thread.interrupted()) { a++; } a += i; } return a; }
    static long i_queue(int n)      { long a = 0; for (int i = 0; i < n; i++) { Q.add(Long.valueOf(i)); a += Q.poll().longValue(); } return a; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        // warm
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
        System.out.printf("%-24s best=%9.2f ns/op  mean=%9.2f ns/op%n",
                name, best / (double) per, (total / (double) reps) / (double) per);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("empty (baseline)", TimerNativeRungRate::a_empty, n, reps);
        time("AtomicInteger.get", TimerNativeRungRate::b_ai_get, n, reps);
        time("System.nanoTime", TimerNativeRungRate::c_nanotime, n, reps);
        time("AtomicLong.inc+dec", TimerNativeRungRate::d_al_incdec, n, reps);
        time("Long.valueOf+longValue", TimerNativeRungRate::e_box, n, reps);
        time("TimeUnit.toMillis+toNanos", TimerNativeRungRate::f_tounit, n, reps);
        time("ReentrantLock lock+unlock", TimerNativeRungRate::g_lock, n, reps);
        time("Thread.interrupted", TimerNativeRungRate::h_interrupted, n, reps);
        time("LBQ add+poll", TimerNativeRungRate::i_queue, n / 4, reps);
        System.out.println("sink=" + (sink == 0 ? 1 : 0));
    }
}
