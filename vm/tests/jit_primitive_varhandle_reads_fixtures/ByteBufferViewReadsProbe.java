// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.nio.ByteBuffer;

public final class ByteBufferViewReadsProbe {
    static volatile long sink;
    static final Class<?>[] TYPES = {short[].class, char[].class, int[].class,
            long[].class, float[].class, double[].class};
    static long read(VarHandle h, ByteBuffer bytes, int at, int kind) {
        switch (kind) {
            case 0: return (short) h.get(bytes, at);
            case 1: return (char) h.get(bytes, at);
            case 2: return (int) h.get(bytes, at);
            case 3: return (long) h.get(bytes, at);
            case 4: return Float.floatToRawIntBits((float) h.get(bytes, at));
            case 5: return Double.doubleToRawLongBits((double) h.get(bytes, at));
            default: throw new AssertionError(kind);
        }
    }
    static long expected(ByteBuffer bytes, int at, int kind, boolean le) {
        int width = kind < 2 ? 2 : kind == 2 || kind == 4 ? 4 : 8;
        long bits = 0;
        for (int i = 0; i < width; i++) {
            int shift = 8 * (le ? i : width - 1 - i);
            bits |= (long) (bytes.get(at + i) & 255) << shift;
        }
        return kind == 0 ? (short) bits : kind == 1 ? (char) bits
                : kind == 2 || kind == 4 ? (int) bits : bits;
    }
    interface Action { void run() throws Throwable; }
    static String failure(Action action) {
        try { action.run(); return "NONE"; }
        catch (Throwable t) { return t.getClass().getName() + ":" + t.getMessage(); }
    }
    public static void main(String[] args) throws Exception {
        for (int carrier = 0; carrier < 6; carrier++) {
        boolean direct = (carrier & 1) != 0;
        ByteBuffer storage = direct ? ByteBuffer.allocateDirect(64) : ByteBuffer.allocate(64);
        storage.position(7).limit(39);
        ByteBuffer data = storage.slice();
        if (carrier >= 2 && carrier < 4) {
            data.position(3).limit(31);
            data = data.slice();
        }
        // Fill before making a read-only alias. Array offset/address and limit
        // are inherited from the slice, and position must not affect a view.
        for (int i = 0; i < data.limit(); i++) data.put(i, (byte) (i * 31 + 0x81));
        if (carrier >= 4) data = data.asReadOnlyBuffer();
        final ByteBuffer bytes = data;
        data.position(2);

        for (boolean le : new boolean[] {false, true}) {
            for (int kind = 0; kind < TYPES.length; kind++) {
                final int k = kind;
                final VarHandle h = MethodHandles.byteBufferViewVarHandle(TYPES[k],
                        le ? ByteOrder.LITTLE_ENDIAN : ByteOrder.BIG_ENDIAN);
                for (int n = 0; n < 20000; n++) {
                    int at = n % 17;
                    long actual = read(h, bytes, at, k);
                    if (actual != expected(bytes, at, k, le)) throw new AssertionError("view read");
                    sink = actual;
                }
                // Exercise sign bits, signed minima, and NaN payloads without
                // numeric conversions; the view helper returns the raw carrier.
                if (carrier < 4) for (long bits : new long[] {0x80000000L, 0x7fc01234L, 0x7f801234L,
                        Long.MIN_VALUE, 0x7ff8000012345678L, 0x7ff0000012345678L}) {
                    for (int n = 0; n < 8; n++) if (carrier < 4) data.put(1 + n, (byte) (bits >>> (8 * n)));
                    if (read(h, bytes, 1, k) != expected(bytes, 1, k, le)) {
                        throw new AssertionError("raw carrier bits");
                    }
                }
                System.gc();
                int width = k < 2 ? 2 : k == 2 || k == 4 ? 4 : 8;
                int savedLimit = bytes.limit();
                bytes.limit(width - 1);
                System.out.println(k + "/" + le + "/changed-limit=" + failure(() -> read(h, bytes, 0, k)));
                bytes.limit(savedLimit);
                int last = bytes.limit() - width;
                if (read(h, bytes, last, k) != expected(bytes, last, k, le)) {
                    throw new AssertionError("moving array/handle");
                }
                System.out.println(k + "/" + le + "/negative=" + failure(() -> read(h, bytes, -1, k)));
                System.out.println(k + "/" + le + "/bounds=" + failure(() -> read(h, bytes, last + 1, k)));
                System.out.println(k + "/" + le + "/null=" + failure(() -> read(h, null, 0, k)));
                for (int size = 0; size < width; size++) {
                    final ByteBuffer tiny = ByteBuffer.allocateDirect(8).limit(size);
                    System.out.println(k + "/" + le + "/tiny/" + size + "=" + failure(() -> read(h, tiny, 0, k)));
                }
                final VarHandle exact = h.withInvokeExactBehavior();
                if (read(exact, bytes, 1, k) != expected(bytes, 1, k, le)) {
                    throw new AssertionError("exact view");
                }
            }
        }
        final VarHandle ints = MethodHandles.byteBufferViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
        System.out.println("wrong-array=" + failure(() -> { int v = (int) ints.get(new int[8], 0); sink = v; }));
        System.out.println("wrong-return=" + failure(() -> { short v = (short) ints.get(bytes, 0); sink = v; }));
        System.out.println("wrong-return-null=" + failure(() -> { short v = (short) ints.get((ByteBuffer) null, 0); sink = v; }));
        System.out.println("wrong-return-array=" + failure(() -> { short v = (short) ints.get(new int[8], 0); sink = v; }));
        System.out.println("wrong-return-index=" + failure(() -> { short v = (short) ints.get(bytes, (Object) "bad"); sink = v; }));
        System.out.println("widen-return=" + failure(() -> { long v = (long) ints.get(bytes, 0); sink = v; }));
        System.out.println("boxed-index=" + failure(() -> { int v = (int) ints.get(bytes, Integer.valueOf(1)); sink = v; }));
        }
        System.out.println("BYTE-BUFFER-VIEW-READS-OK");
    }
}
