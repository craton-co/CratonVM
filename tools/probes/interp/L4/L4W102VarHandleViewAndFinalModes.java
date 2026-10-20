// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L4 (hunt): VarHandle access-mode
// refusals against HotSpot 25, both modes. Over a real JDK the factories
// (`byteArrayViewVarHandle`, `byteBufferViewVarHandle`, `findVarHandle`,
// `arrayElementVarHandle`, ...) are JDK bytecode (the `JdkInvoke` row); only
// the signature-polymorphic access modes are CratonVM natives
// (`lang_invoke::varhandle_*`, serving the real handles through adopted
// rows). This pins their refusals:
//
// * a `byte[]` view: plain get/set at any index, every other mode
//   UnsupportedOperationException (JDK 22+, JDK-8318966);
// * a `ByteBuffer` view: heap buffer `IllegalStateException` first, then
//   `ReadOnlyBufferException` for a write, then the index, then the
//   misaligned `IllegalStateException`; `short` views only the ordered
//   reads/writes; `float` views no `getAndAdd`;
// * a final field: the four read modes answer, every write mode is
//   UnsupportedOperationException;
// * the factories' own argument refusals.
//
// Every row prints the answer or `exception-class: message`. By reading no
// row differs today; this is a regression pin for the doors (no fix).
//
// Run: javac -d . L4W102VarHandleViewAndFinalModes.java
//      java -cp . L4W102VarHandleViewAndFinalModes                          (HotSpot)
//      cratonvm --jdk-only [--nojit] -cp . L4W102VarHandleViewAndFinalModes
//      cratonvm --compatible [--nojit] -cp . L4W102VarHandleViewAndFinalModes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources, unmeasured where marked ?)
// array-view-get-misaligned 16909060
// array-view-set-misaligned 1
// array-view-getVolatile java.lang.UnsupportedOperationException: getVolatile
// array-view-compareAndSet java.lang.UnsupportedOperationException: compareAndSet
// array-view-getAndAdd java.lang.UnsupportedOperationException: getAndAdd
// heap-buffer-getVolatile java.lang.IllegalStateException: Atomic access not supported for heap buffer
// heap-ro-set java.nio.ReadOnlyBufferException: null
// heap-ro-setVolatile java.lang.IllegalStateException: Atomic access not supported for heap buffer
// direct-getVolatile-misaligned java.lang.IllegalStateException: Misaligned access at index: 1
// direct-getVolatile-oob java.lang.IndexOutOfBoundsException: Index 16 out of bounds for length 13
// direct-ro-getVolatile 0
// direct-ro-setVolatile java.nio.ReadOnlyBufferException: null
// direct-ro-setVolatile-misaligned java.nio.ReadOnlyBufferException: null
// direct-getAndAdd 0 then 5
// direct-getAndBitwiseOr 5 then 245
// direct-short-getVolatile 0
// direct-short-compareAndSet java.lang.UnsupportedOperationException: compareAndSet
// direct-float-getAndAdd java.lang.UnsupportedOperationException: getAndAdd
// direct-float-compareAndSet true
// final-get 5 5 5 5
// final-set java.lang.UnsupportedOperationException: set
// final-setOpaque java.lang.UnsupportedOperationException: setOpaque
// final-setRelease java.lang.UnsupportedOperationException: setRelease
// final-compareAndSet java.lang.UnsupportedOperationException: compareAndSet
// final-getAndAdd java.lang.UnsupportedOperationException: getAndAdd
// final-supported SET false GET_OPAQUE true
// static-final-getVolatile 9
// static-final-setVolatile java.lang.UnsupportedOperationException: setVolatile
// arrayElementVarHandle-int java.lang.IllegalArgumentException: not an array class: int
// arrayElementVarHandle-null java.lang.NullPointerException
// byteArrayView-int java.lang.IllegalArgumentException: not an array class: int (?)
// byteArrayView-byte java.lang.UnsupportedOperationException: (?)
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class L4W102VarHandleViewAndFinalModes {
    static class Holder {
        final int f = 5;
    }

    static final int SF = 9;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + " " + r.run());
        } catch (Throwable e) {
            System.out.println(name + " " + e.getClass().getName() + ": " + e.getMessage());
        }
    }

    public static void main(String[] args) throws Throwable {
        VarHandle av = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        byte[] ba = new byte[16];
        for (int i = 0; i < ba.length; i++) {
            ba[i] = (byte) i;
        }
        row("array-view-get-misaligned", () -> (int) av.get(ba, 1));
        row("array-view-set-misaligned", () -> {
            byte[] b2 = new byte[8];
            av.set(b2, 3, 0x01000000);
            return b2[3];
        });
        row("array-view-getVolatile", () -> (int) av.getVolatile(ba, 0));
        row("array-view-compareAndSet", () -> (boolean) av.compareAndSet(ba, 0, 0, 1));
        row("array-view-getAndAdd", () -> (int) av.getAndAdd(ba, 0, 1));

        VarHandle bv = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        ByteBuffer heap = ByteBuffer.allocate(16);
        ByteBuffer heapRo = heap.asReadOnlyBuffer();
        row("heap-buffer-getVolatile", () -> (int) bv.getVolatile(heap, 0));
        row("heap-ro-set", () -> {
            bv.set(heapRo, 0, 1);
            return "stored";
        });
        row("heap-ro-setVolatile", () -> {
            bv.setVolatile(heapRo, 0, 1);
            return "stored";
        });
        ByteBuffer direct = ByteBuffer.allocateDirect(16);
        ByteBuffer directRo = direct.asReadOnlyBuffer();
        row("direct-getVolatile-misaligned", () -> (int) bv.getVolatile(direct, 1));
        row("direct-getVolatile-oob", () -> (int) bv.getVolatile(direct, 16));
        row("direct-ro-getVolatile", () -> (int) bv.getVolatile(directRo, 0));
        row("direct-ro-setVolatile", () -> {
            bv.setVolatile(directRo, 0, 1);
            return "stored";
        });
        row("direct-ro-setVolatile-misaligned", () -> {
            bv.setVolatile(directRo, 1, 1);
            return "stored";
        });
        row("direct-getAndAdd", () -> {
            int old = (int) bv.getAndAdd(direct, 0, 5);
            return old + " then " + (int) bv.getVolatile(direct, 0);
        });
        row("direct-getAndBitwiseOr", () -> {
            int old = (int) bv.getAndBitwiseOr(direct, 0, 0xF0);
            return old + " then " + (int) bv.getVolatile(direct, 0);
        });
        VarHandle sv = MethodHandles.byteBufferViewVarHandle(short[].class, ByteOrder.BIG_ENDIAN);
        row("direct-short-getVolatile", () -> (short) sv.getVolatile(direct, 8));
        row("direct-short-compareAndSet", () -> (boolean) sv.compareAndSet(direct, 8, (short) 0, (short) 1));
        VarHandle fv = MethodHandles.byteBufferViewVarHandle(float[].class, ByteOrder.BIG_ENDIAN);
        row("direct-float-getAndAdd", () -> (float) fv.getAndAdd(direct, 8, 1.0f));
        row("direct-float-compareAndSet", () -> (boolean) fv.compareAndSet(direct, 8, 0.0f, 2.5f));

        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle fin = l.findVarHandle(Holder.class, "f", int.class);
        Holder h = new Holder();
        row("final-get", () -> (int) fin.get(h) + " " + (int) fin.getOpaque(h) + " "
                + (int) fin.getAcquire(h) + " " + (int) fin.getVolatile(h));
        row("final-set", () -> {
            fin.set(h, 6);
            return "stored " + h.f;
        });
        row("final-setOpaque", () -> {
            fin.setOpaque(h, 6);
            return "stored";
        });
        row("final-setRelease", () -> {
            fin.setRelease(h, 6);
            return "stored";
        });
        row("final-compareAndSet", () -> (boolean) fin.compareAndSet(h, 5, 6));
        row("final-getAndAdd", () -> (int) fin.getAndAdd(h, 1));
        row("final-supported", () -> "SET " + fin.isAccessModeSupported(VarHandle.AccessMode.SET)
                + " GET_OPAQUE " + fin.isAccessModeSupported(VarHandle.AccessMode.GET_OPAQUE));
        VarHandle sfin = l.findStaticVarHandle(L4W102VarHandleViewAndFinalModes.class, "SF", int.class);
        row("static-final-getVolatile", () -> (int) sfin.getVolatile());
        row("static-final-setVolatile", () -> {
            sfin.setVolatile(10);
            return "stored";
        });

        row("arrayElementVarHandle-int", () -> MethodHandles.arrayElementVarHandle(int.class));
        try {
            MethodHandles.arrayElementVarHandle(null);
            System.out.println("arrayElementVarHandle-null no exception");
        } catch (Throwable e) {
            System.out.println("arrayElementVarHandle-null " + e.getClass().getName());
        }
        row("byteArrayView-int", () -> MethodHandles.byteArrayViewVarHandle(int.class, ByteOrder.BIG_ENDIAN));
        row("byteArrayView-byte", () -> MethodHandles.byteArrayViewVarHandle(byte[].class, ByteOrder.BIG_ENDIAN));
    }
}
