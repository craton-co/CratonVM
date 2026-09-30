// Interpreter round i1 wave 31 — byte-array and ByteBuffer view VarHandles:
// which access modes they admit, the checks before an atomic mode, the atomic
// modes on a direct buffer, and `isAccessModeSupported`.
//
// Page: docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md
// (items 1, 2 and 5).
//
// Before wave 31, CratonVM: a `byte[]` view answered every mode (its
// read-modify-write modes on ONE byte); a direct `ByteBuffer` view's atomic
// modes answered `false`/`null`/`0` without writing (a CAS loop over it never
// ended); `isAccessModeSupported` threw `NullPointerException` on every handle
// the VM mints.
//
// Run (no setup):
//   javac -d out L4W31VarHandleViews.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L4W31VarHandleViews
// and compare verbatim with `java -cp out L4W31VarHandleViews` (HotSpot 25).

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.VarHandle.AccessMode;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.concurrent.Callable;

public class L4W31VarHandleViews {
    int i;
    boolean z;
    static final long FINAL = 1L;

    static void row(String label, Callable<Object> c) {
        try {
            System.out.println(label + ": " + c.call());
        } catch (Throwable t) {
            System.out.println(label + ": " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    public static void main(String[] args) throws Throwable {
        VarHandle ai = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
        byte[] bytes = new byte[16];
        row("array set/get", () -> { ai.set(bytes, 4, 0x01020304); return (int) ai.get(bytes, 4) + " " + bytes[4]; });
        row("array getVolatile", () -> (int) ai.getVolatile(bytes, 4));
        row("array setRelease", () -> { ai.setRelease(bytes, 4, 7); return "stored"; });
        row("array compareAndSet", () -> (boolean) ai.compareAndSet(bytes, 4, 0x01020304, 9));
        row("array getAndAdd", () -> (int) ai.getAndAdd(bytes, 4, 1));
        row("array after", () -> (int) ai.get(bytes, 4));

        VarHandle bi = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle bl = MethodHandles.byteBufferViewVarHandle(long[].class, ByteOrder.BIG_ENDIAN);
        VarHandle bs = MethodHandles.byteBufferViewVarHandle(short[].class, ByteOrder.LITTLE_ENDIAN);
        VarHandle bd = MethodHandles.byteBufferViewVarHandle(double[].class, ByteOrder.LITTLE_ENDIAN);
        ByteBuffer heap = ByteBuffer.allocate(16);
        row("heap get", () -> (int) bi.get(heap, 0));
        row("heap getVolatile", () -> (int) bi.getVolatile(heap, 0));
        row("heap compareAndSet", () -> (boolean) bi.compareAndSet(heap, 0, 0, 1));

        ByteBuffer direct = ByteBuffer.allocateDirect(32);
        row("direct compareAndSet", () -> (boolean) bi.compareAndSet(direct, 0, 0, 5));
        row("direct compareAndSet stale", () -> (boolean) bi.compareAndSet(direct, 0, 0, 6));
        row("direct get", () -> (int) bi.get(direct, 0) + " byte0=" + direct.get(0));
        row("direct getAndAdd", () -> (int) bi.getAndAdd(direct, 0, 3));
        row("direct getAndBitwiseOr", () -> (int) bi.getAndBitwiseOr(direct, 0, 0x100));
        row("direct getAndSet", () -> (int) bi.getAndSet(direct, 0, 42));
        row("direct compareAndExchange", () -> (int) bi.compareAndExchange(direct, 0, 42, 43));
        row("direct getVolatile", () -> (int) bi.getVolatile(direct, 0));
        row("direct misaligned", () -> (boolean) bi.compareAndSet(direct, 1, 0, 1));
        row("direct out of bounds", () -> (boolean) bi.compareAndSet(direct, 32, 0, 1));
        row("direct long BE cas", () -> (boolean) bl.compareAndSet(direct, 8, 0L, 0x0102030405060708L)
                + " byte8=" + direct.get(8) + " byte15=" + direct.get(15));
        row("direct long getAndAdd", () -> (long) bl.getAndAdd(direct, 8, 1L));
        row("direct long after", () -> Long.toHexString((long) bl.get(direct, 8)));
        row("direct double cas", () -> (boolean) bd.compareAndSet(direct, 16, 0.0d, 2.5d));
        row("direct double get", () -> (double) bd.get(direct, 16));
        row("direct double getAndAdd", () -> (double) bd.getAndAdd(direct, 16, 1.0d));
        row("direct short getOpaque", () -> (short) bs.getOpaque(direct, 24));
        row("direct short cas", () -> (boolean) bs.compareAndSet(direct, 24, (short) 0, (short) 1));
        ByteBuffer ro = direct.asReadOnlyBuffer();
        row("read-only cas", () -> (boolean) bi.compareAndSet(ro, 0, 43, 44));
        row("read-only getVolatile", () -> (int) bi.getVolatile(ro, 0));

        VarHandle fi = MethodHandles.lookup().findVarHandle(L4W31VarHandleViews.class, "i", int.class);
        VarHandle fz = MethodHandles.lookup().findVarHandle(L4W31VarHandleViews.class, "z", boolean.class);
        VarHandle ff = MethodHandles.lookup().unreflectVarHandle(L4W31VarHandleViews.class.getDeclaredField("FINAL"));
        row("supported int GET_AND_ADD", () -> fi.isAccessModeSupported(AccessMode.GET_AND_ADD));
        row("supported boolean GET_AND_ADD", () -> fz.isAccessModeSupported(AccessMode.GET_AND_ADD));
        row("supported boolean GET_AND_BITWISE_OR", () -> fz.isAccessModeSupported(AccessMode.GET_AND_BITWISE_OR));
        row("supported final GET", () -> ff.isAccessModeSupported(AccessMode.GET));
        row("supported final SET", () -> ff.isAccessModeSupported(AccessMode.SET));
        row("supported array view GET", () -> ai.isAccessModeSupported(AccessMode.GET));
        row("supported array view GET_VOLATILE", () -> ai.isAccessModeSupported(AccessMode.GET_VOLATILE));
        row("supported buffer int GET_AND_ADD", () -> bi.isAccessModeSupported(AccessMode.GET_AND_ADD));
        row("supported buffer double GET_AND_ADD", () -> bd.isAccessModeSupported(AccessMode.GET_AND_ADD));
        row("supported buffer short COMPARE_AND_SET", () -> bs.isAccessModeSupported(AccessMode.COMPARE_AND_SET));
        row("supported buffer short SET_RELEASE", () -> bs.isAccessModeSupported(AccessMode.SET_RELEASE));
    }
}
