import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

/**
 * The rung the `--dump-native-registry` census puts at the top of netty's
 * snappy phase: `java/lang/invoke/VarHandle.get` on an ordinary `int` instance
 * field, 21 368 822 calls per 4 MiB through `NettyZipBombPhases snappy`
 * (~5.1 per output byte).
 *
 * That is netty 4.2's reference-count check verbatim —
 * `RefCnt.VarHandleRefCnt.isLiveNonVolatile` is `(int) VH.get(instance)` and
 * `refCnt()` is `(int) VH.getAcquire(instance) >>> 1` — so `refcnt_shape`
 * below is not a model of the workload, it is the workload's inner two lines.
 *
 * Every arm is its own small method called REPS times, never a loop inline in
 * `main`, which on this VM measures the interpreter.
 *
 * Measure as a SAME-BINARY A/B on the kill switch:
 *
 *   cratonvm --java-home <jdk> -cp <out> VarHandleReadRate 4000000 40
 *   CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=0 cratonvm ... VarHandleReadRate 4000000 40
 *
 * Do NOT compare against a binary built from a different commit — that
 * shortcut manufactured 13-19 % "regressions" on phases containing neither
 * call the last time it was taken (see `HotNativeRungRate`).
 *
 * `field_read` is the control: the SAME field read as plain bytecode, with no
 * `VarHandle` anywhere. It is what the bound arm is being walked toward, and
 * it must not move between arms — if it does, the two arms differ by something
 * other than the gate.
 *
 * `array_element` is the second control, and it is the one that says the bind
 * stayed inside its scope: an array-element handle carries two coordinates, so
 * `varhandle_read_helper_slot` refuses it and it keeps the generic dispatch in
 * BOTH arms. A change there is a bind that escaped its descriptor filter.
 */
public final class VarHandleReadRate {

    static final class RefCntLike {
        int value = 2;
        long wide = 0x0102030405060708L;
    }

    static long sink;
    static final RefCntLike H = new RefCntLike();
    static final int[] ARR = new int[64];

    static final VarHandle VI;
    static final VarHandle VJ;
    static final VarHandle VAE;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VI = l.findVarHandle(RefCntLike.class, "value", int.class);
            VJ = l.findVarHandle(RefCntLike.class, "wide", long.class);
            VAE = MethodHandles.arrayElementVarHandle(int[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
        ARR[7] = 4242;
    }

    static long a_empty(int n)     { long a=0; for (int i=0;i<n;i++) { a += i; } return a; }
    static long b_field_read(int n){ long a=0; for (int i=0;i<n;i++) { a += H.value; } return a; }
    static long c_get_int(int n)   { long a=0; for (int i=0;i<n;i++) { a += (int) VI.get(H); } return a; }
    static long d_acquire_int(int n){long a=0; for (int i=0;i<n;i++) { a += (int) VI.getAcquire(H); } return a; }
    static long e_get_long(int n)  { long a=0; for (int i=0;i<n;i++) { a += (long) VJ.get(H); } return a; }
    static long f_array_elem(int n){ long a=0; for (int i=0;i<n;i++) { a += (int) VAE.get(ARR, 7); } return a; }

    /**
     * netty's own two lines. `isLiveNonVolatile` reads the raw count with a
     * plain `get` and tests two bits; `refCnt()` reads it with `getAcquire`
     * and shifts. Both run per buffer accessor.
     */
    static long g_refcnt_shape(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            int raw = (int) VI.get(H);
            boolean live = raw == 2 || (raw & 1) == 0;
            a += live ? ((int) VI.getAcquire(H)) >>> 1 : 0;
        }
        return a;
    }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int w = 0; w < reps; w++) { sink += arm.run(per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { sink += arm.run(per); }
        long t1 = System.nanoTime();
        System.out.printf("%-28s %9.2f ns/op%n", name, (double) (t1 - t0) / (per * (long) reps));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("empty (control)", VarHandleReadRate::a_empty, n, reps);
        time("field read (control)", VarHandleReadRate::b_field_read, n, reps);
        time("VH.get int", VarHandleReadRate::c_get_int, n, reps);
        time("VH.getAcquire int", VarHandleReadRate::d_acquire_int, n, reps);
        time("VH.get long", VarHandleReadRate::e_get_long, n, reps);
        time("VH.get array elem (control)", VarHandleReadRate::f_array_elem, n, reps);
        time("netty refCnt shape", VarHandleReadRate::g_refcnt_shape, n, reps);
        // Print a value from every handle, so an arm that stopped answering
        // cannot be read as an arm that got fast.
        System.out.println("values = " + (int) VI.get(H) + " " + (long) VJ.get(H)
                + " " + (int) VAE.get(ARR, 7) + " sink=" + sink);
    }
}
