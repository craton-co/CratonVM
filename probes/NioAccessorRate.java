import java.nio.ByteBuffer;

/**
 * Per-call cost of `java.nio.ByteBuffer`'s absolute accessors, which is what
 * netty's non-Unsafe `PooledDirectByteBuf._setLong` / `_setByte` call.
 *
 * Measured 2026-08-17 (real-JDK mode, G1), HotSpot 25 on the same host:
 *
 *   byte[] store                      0.16 ns HotSpot /    4.53 ns CratonVM
 *   direct ByteBuffer.put(int,byte)   0.29 ns /  282 ns
 *   direct ByteBuffer.putLong(int,J)  0.30 ns / 1088 ns
 *   heap   ByteBuffer.putLong(int,J)  0.28 ns / 1058 ns
 *   direct ByteBuffer.getLong(int)    0.49 ns / 1430 ns
 *
 * The row that decides the diagnosis is `putLong` against `put(byte)` on the
 * SAME buffer. It looks like per-byte work — and the natives really did resolve
 * the buffer's storage once per byte — but making them resolve ONCE moved these
 * numbers not at all (918 -> 943 and 864 -> 877 ns/op, interleaved, two rounds).
 * The ratio is ~3.5x, not 8x, because the cost is ONE NATIVE CALL plus a few
 * name-keyed field reads, not eight byte stores. A single-byte `put` already
 * costs ~260 ns for one native and one stored byte; that ~260 ns floor is the
 * finding, and it is what makes `HttpContentDecompressorTest.testZipBomb`
 * exceed its wall (netty's `writeZero` is 131 072 `_setLong` per MiB, 256 MiB).
 *
 * Every arm is its own small method called REPS times — never a loop inline in
 * `main`, which on this VM measures the interpreter instead.
 */
public final class NioAccessorRate {
    static long sink;
    static final ByteBuffer D = ByteBuffer.allocateDirect(1 << 20);
    static final ByteBuffer H = ByteBuffer.allocate(1 << 20);
    static final byte[] A = new byte[1 << 20];

    static long a_arrayStore(int n)   { for (int i=0;i<n;i++) { A[i & 0xFFFFF] = 0; } return 0; }
    static long b_directPutByte(int n){ for (int i=0;i<n;i++) { D.put(i & 0xFFFFF, (byte) 0); } return 0; }
    static long c_heapPutByte(int n)  { for (int i=0;i<n;i++) { H.put(i & 0xFFFFF, (byte) 0); } return 0; }
    static long d_directPutLong(int n){ for (int i=0;i<n;i++) { D.putLong((i & 0xFFFF) << 3, 0L); } return 0; }
    static long e_heapPutLong(int n)  { for (int i=0;i<n;i++) { H.putLong((i & 0xFFFF) << 3, 0L); } return 0; }
    static long f_directGetLong(int n){ long a=0; for (int i=0;i<n;i++) { a += D.getLong((i & 0xFFFF) << 3); } return a; }
    static long g_directPutInt(int n) { for (int i=0;i<n;i++) { D.putInt((i & 0x3FFFF) << 2, 0); } return 0; }

    interface Arm { long run(int n); }

    static void time(String name, Arm arm, int n, int reps) {
        int per = n / reps;
        for (int w = 0; w < reps; w++) { sink += arm.run(per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { sink += arm.run(per); }
        long t1 = System.nanoTime();
        System.out.printf("%-30s %9.2f ns/op%n", name, (double) (t1 - t0) / (per * (long) reps));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        time("byte[] store",             NioAccessorRate::a_arrayStore,    n, reps);
        time("direct ByteBuffer.put(b)", NioAccessorRate::b_directPutByte, n, reps);
        time("heap ByteBuffer.put(b)",   NioAccessorRate::c_heapPutByte,   n, reps);
        time("direct ByteBuffer.putInt", NioAccessorRate::g_directPutInt,  n, reps);
        time("direct ByteBuffer.putLong",NioAccessorRate::d_directPutLong, n, reps);
        time("heap ByteBuffer.putLong",  NioAccessorRate::e_heapPutLong,   n, reps);
        time("direct ByteBuffer.getLong",NioAccessorRate::f_directGetLong, n, reps);
        System.out.println("sink=" + sink);
    }
}
