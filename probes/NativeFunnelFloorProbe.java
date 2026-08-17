import java.security.MessageDigest;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * The per-registered-native-call floor from COMPILED code, split by what the
 * funnel actually does per call.
 *
 * `docs/known-issues/perf/vm-per-call-dispatch-cost-20260813.md` names this
 * floor — ~150-210 ns per registered native call — as the residual of three
 * netty suite classes that stay over the 180 s per-class cap, and it names
 * `MessageDigest.update(byte)` as the clean single-call specimen: 6.4 ns on
 * HotSpot 25 against ~170-210 ns here. This probe is the rung set that says
 * WHICH part of the funnel that is, in one process, so the answer survives a
 * loaded host (§6: a ratio taken in the same process is the one figure immune
 * to load).
 *
 * The split that matters is LEAF vs non-leaf. `safe_native_call_leaf` drops
 * argument pinning, the STW probe, both GC-pressure hooks, the two
 * `record_transition`s, the JNI drain and the pin ring
 * (`NativeMethodRegistry::set_leaf`); `safe_native_call_impl` pays all of it.
 * `AtomicInteger.get` is leaf-registered and `AtomicInteger.compareAndSet` is
 * deliberately NOT (it takes the monitor table's per-object CAS lock), so the
 * two bracket the funnel's fixed non-leaf cost with the same receiver, the same
 * arity and the same call site shape.
 *
 * Every arm is its own small method called `reps` times, never a loop inline in
 * `main`: compile order binds a call site permanently on this VM, so a rung
 * whose loop is built before its callee warms measures the cold arm forever.
 *
 *   cratonvm --java-home <jdk25> -cp <out> NativeFunnelFloorProbe 4000000 40
 */
public final class NativeFunnelFloorProbe {

    static long sink;
    static Object osink;
    static final Object O = new Object();
    static final AtomicInteger AI = new AtomicInteger(7);
    static MessageDigest MD;

    /** The control: a user-written call of the same shape, no native at all. */
    static int plain(int i) { return i & 0xFFFF; }

    static long a_empty(int n)        { long a=0; for (int i=0;i<n;i++) { a += i; } return a; }
    static long b_plainCall(int n)    { long a=0; for (int i=0;i<n;i++) { a += plain(i); } return a; }
    static long c_atomicGet(int n)    { long a=0; for (int i=0;i<n;i++) { a += AI.get(); } return a; }
    static long d_atomicCas(int n)    { long a=0; for (int i=0;i<n;i++) { if (AI.compareAndSet(7,7)) a++; } return a; }
    static long e_identityHash(int n) { long a=0; for (int i=0;i<n;i++) { a += System.identityHashCode(O); } return a; }
    static long f_nanoTime(int n)     { long a=0; for (int i=0;i<n;i++) { a += System.nanoTime(); } return a; }
    static long g_mdUpdateByte(int n) { long a=0; for (int i=0;i<n;i++) { MD.update((byte) i); a += i; } return a; }
    static long h_currentThread(int n){ Object o=null; for (int i=0;i<n;i++) { o = Thread.currentThread(); } osink=o; return 1; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int w = 0; w < reps; w++) { sink += arm.run(per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { sink += arm.run(per); }
        long t1 = System.nanoTime();
        System.out.printf("%-34s %9.2f ns/op%n", name, (double) (t1 - t0) / (per * (long) reps));
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        MD = MessageDigest.getInstance("SHA-256");

        time("control: no call",            NativeFunnelFloorProbe::a_empty, n, reps);
        time("control: plain Java call",    NativeFunnelFloorProbe::b_plainCall, n, reps);
        time("LEAF  AtomicInteger.get",     NativeFunnelFloorProbe::c_atomicGet, n, reps);
        time("FUNNEL AtomicInteger.CAS",    NativeFunnelFloorProbe::d_atomicCas, n, reps);
        time("FUNNEL identityHashCode",     NativeFunnelFloorProbe::e_identityHash, n, reps);
        time("FUNNEL System.nanoTime",      NativeFunnelFloorProbe::f_nanoTime, n, reps);
        time("FUNNEL MessageDigest.update", NativeFunnelFloorProbe::g_mdUpdateByte, n, reps);
        time("Thread.currentThread",        NativeFunnelFloorProbe::h_currentThread, n, reps);

        // Not decoration: a fast path that broke the digest contract would
        // still print the numbers above. This is the SHA-256 of the 8 bytes
        // 0..7, which must match HotSpot byte for byte.
        MessageDigest fresh = MessageDigest.getInstance("SHA-256");
        for (int i = 0; i < 8; i++) { fresh.update((byte) i); }
        StringBuilder sb = new StringBuilder();
        for (byte b : fresh.digest()) { sb.append(String.format("%02x", b)); }
        System.out.println("sha256(0..7)=" + sb);
        System.out.println("sink=" + sink + " osink=" + (osink != null));
    }
}
