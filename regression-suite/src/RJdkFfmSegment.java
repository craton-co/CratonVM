import java.lang.foreign.*;
import java.util.*;

/**
 * G6-1: the FFM (java.lang.foreign) surface measured against HotSpot
 * 25.0.3+9-LTS, 2026-08-16. Every expectation here is a transcribed oracle
 * value from that record's probes ({@code FfmProbe}, {@code FfmProbe2},
 * {@code FfmProbe3}, {@code FfmProbe4}), which ran 86 checks, 86 green, zero
 * FAIL lines on the oracle itself -- so a failure on CratonVM is a VM
 * divergence, not a bad expectation.
 *
 * <p>Covers: {@code maxByteAlignment()} being the element TYPE and not a flat
 * 8 (§1); {@code toArray}/{@code elements}/{@code spliterator} on a heap
 * receiver, including the fix for {@code toArray} silently reading zeros off
 * address 0 instead of the backing array (§2-3); {@code asReadOnly()}
 * withholding {@code heapBase()} while still permitting reads (§6); the
 * {@code asSlice} rule, its two corrected messages, and message ordering
 * (§5); the corrected heap {@code get}/{@code set} alignment gate, which used
 * to be stricter than the oracle (§7); and the layout factories' overflow
 * exception class, element-count storage, and null-member refusal (§8).
 */
public class RJdkFfmSegment {
    static int checks = 0;
    static final int EXPECTED_CHECKS = 77;

    static void eq(String label, Object actual, Object expected) {
        checks++;
        String a = str(actual);
        String e = str(expected);
        if (!a.equals(e)) {
            throw new AssertionError(label + ": expected=[" + e + "] actual=[" + a + "]");
        }
    }

    static String str(Object o) {
        if (o instanceof byte[] b) return Arrays.toString(b);
        if (o instanceof int[] i) return Arrays.toString(i);
        return String.valueOf(o);
    }

    static Object attempt(java.util.function.Supplier<Object> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    /**
     * The exception CLASS alone, for rows whose message embeds a receiver
     * toString() with an identity hash no VM can be asked to reproduce.
     */
    static String exClass(java.util.function.Supplier<Object> s) {
        try {
            s.get();
            return "no exception";
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    public static void main(String[] args) {
        // --- maxByteAlignment is the element type, not 8 ---
        eq("A1 byte[16] maxAlign", MemorySegment.ofArray(new byte[16]).maxByteAlignment(), 1L);
        eq("A2 short[8] maxAlign", MemorySegment.ofArray(new short[8]).maxByteAlignment(), 2L);
        eq("A3 char[8] maxAlign", MemorySegment.ofArray(new char[8]).maxByteAlignment(), 2L);
        eq("A4 int[8] maxAlign", MemorySegment.ofArray(new int[8]).maxByteAlignment(), 4L);
        eq("A5 long[8] maxAlign", MemorySegment.ofArray(new long[8]).maxByteAlignment(), 8L);
        eq("A6 float[8] maxAlign", MemorySegment.ofArray(new float[8]).maxByteAlignment(), 4L);
        eq("A7 double[8] maxAlign", MemorySegment.ofArray(new double[8]).maxByteAlignment(), 8L);
        eq("A8 byte[0] maxAlign", MemorySegment.ofArray(new byte[0]).maxByteAlignment(), 1L);
        eq("A9 long[0] maxAlign", MemorySegment.ofArray(new long[0]).maxByteAlignment(), 8L);
        MemorySegment l4 = MemorySegment.ofArray(new long[4]);
        eq("A10 long[4] slice4 maxAlign", l4.asSlice(4).maxByteAlignment(), 4L);
        eq("A11 long[4] slice8 maxAlign", l4.asSlice(8).maxByteAlignment(), 8L);
        eq("A12 long[4] slice12 maxAlign", l4.asSlice(12).maxByteAlignment(), 4L);
        eq(
                "A13 int[8] slice2 maxAlign",
                MemorySegment.ofArray(new int[8]).asSlice(2).maxByteAlignment(),
                2L);
        eq("A14 NULL maxAlign", MemorySegment.NULL.maxByteAlignment(), 4611686018427387904L);
        eq("A15 ofAddress(12) maxAlign", MemorySegment.ofAddress(12).maxByteAlignment(), 4L);

        // --- toArray on a heap receiver reads the array, not address 0 ---
        MemorySegment b16 =
                MemorySegment.ofArray(
                        new byte[] {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15});
        MemorySegment i8 = MemorySegment.ofArray(new int[] {10, 20, 30, 40, 50, 60, 70, 80});
        eq(
                "B1 byte[16] toArray",
                b16.toArray(ValueLayout.JAVA_BYTE),
                new byte[] {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15});
        eq(
                "B2 slice(3,4) toArray",
                b16.asSlice(3, 4).toArray(ValueLayout.JAVA_BYTE),
                new byte[] {3, 4, 5, 6});
        eq(
                "B3 int[8] toArray",
                i8.toArray(ValueLayout.JAVA_INT),
                new int[] {10, 20, 30, 40, 50, 60, 70, 80});
        eq("B4 int[8] toArray(JAVA_BYTE).length", i8.toArray(ValueLayout.JAVA_BYTE).length, 32);
        eq(
                "B5 byte[16] toArray(JAVA_INT_UNALIGNED)",
                b16.toArray(ValueLayout.JAVA_INT_UNALIGNED),
                new int[] {50462976, 117835012, 185207048, 252579084});
        eq(
                "B6 byte[16] toArray(JAVA_INT)",
                attempt(() -> b16.toArray(ValueLayout.JAVA_INT)),
                "java.lang.IllegalArgumentException: Source segment incompatible with alignment constraints");
        eq(
                "B7 byte[15] toArray(JAVA_INT)",
                attempt(() -> MemorySegment.ofArray(new byte[15]).toArray(ValueLayout.JAVA_INT)),
                "java.lang.IllegalStateException: Segment size is not a multiple of 4. Size: 15");
        eq(
                "B8 int[8] toArray(JAVA_LONG)",
                attempt(() -> i8.toArray(ValueLayout.JAVA_LONG)),
                "java.lang.IllegalArgumentException: Source segment incompatible with alignment constraints");

        // --- elements / spliterator on a heap receiver ---
        eq(
                "C1 byte[16] elements(JAVA_BYTE).count",
                b16.elements(ValueLayout.JAVA_BYTE).count(),
                16L);
        eq(
                "C2 byte[16] elements(JAVA_INT)",
                attempt(() -> b16.elements(ValueLayout.JAVA_INT).count()),
                "java.lang.IllegalArgumentException: Incompatible alignment constraints");
        eq(
                "C3 byte[16] elements(JAVA_INT_UNALIGNED).count",
                b16.elements(ValueLayout.JAVA_INT_UNALIGNED).count(),
                4L);
        eq("C4 int[8] elements(JAVA_INT).count", i8.elements(ValueLayout.JAVA_INT).count(), 8L);
        eq(
                "C5 int[8] elements values",
                i8.elements(ValueLayout.JAVA_INT)
                        .map(s -> s.get(ValueLayout.JAVA_INT, 0))
                        .toList()
                        .toString(),
                "[10, 20, 30, 40, 50, 60, 70, 80]");
        eq(
                "C6 int[8] element isNative",
                i8.elements(ValueLayout.JAVA_INT).findFirst().get().isNative(),
                false);
        eq(
                "C7 int[8] element heapBase present",
                i8.elements(ValueLayout.JAVA_INT).findFirst().get().heapBase().isPresent(),
                true);
        eq(
                "C8 int[8] element 1 address",
                i8.elements(ValueLayout.JAVA_INT).skip(1).findFirst().get().address(),
                4L);
        eq(
                "C9 int[8] slice4 elements count",
                i8.asSlice(4).elements(ValueLayout.JAVA_INT).count(),
                7L);
        eq(
                "C10 byte[16] spliterator(JAVA_BYTE) estimateSize",
                b16.spliterator(ValueLayout.JAVA_BYTE).estimateSize(),
                16L);
        eq(
                "C11 int[8] spliterator(JAVA_INT) estimateSize",
                i8.spliterator(ValueLayout.JAVA_INT).estimateSize(),
                8L);
        long[] sum = new long[1];
        i8.spliterator(ValueLayout.JAVA_INT)
                .forEachRemaining(s -> sum[0] += s.get(ValueLayout.JAVA_INT, 0));
        eq("C12 int[8] spliterator sum", sum[0], 360L);
        eq(
                "C13 byte[15] elements(JAVA_INT_UNALIGNED)",
                attempt(
                        () ->
                                MemorySegment.ofArray(new byte[15])
                                        .elements(ValueLayout.JAVA_INT_UNALIGNED)
                                        .count()),
                "java.lang.IllegalArgumentException: Segment size is not a multiple of layout size");
        eq(
                "C14 byte[16] elements(structLayout()) zero size",
                attempt(() -> b16.elements(MemoryLayout.structLayout()).count()),
                "java.lang.IllegalArgumentException: Element layout size cannot be zero");

        // --- read-only withholds the backing array, but still reads ---
        MemorySegment ro = b16.asReadOnly();
        eq("D1 writable heapBase present", b16.heapBase().isPresent(), true);
        eq("D2 readOnly heapBase present", ro.heapBase().isPresent(), false);
        eq("D3 readOnly slice heapBase", ro.asSlice(3, 4).heapBase().isPresent(), false);
        eq("D4 readOnly slice isReadOnly", ro.asSlice(3, 4).isReadOnly(), true);
        eq(
                "D5 readOnly element heapBase",
                ro.elements(ValueLayout.JAVA_BYTE).findFirst().get().heapBase().isPresent(),
                false);
        eq("D6 readOnly toArray length", ro.toArray(ValueLayout.JAVA_BYTE).length, 16);
        eq(
                "D7 readOnly set",
                attempt(
                        () -> {
                            ro.set(ValueLayout.JAVA_BYTE, 0, (byte) 1);
                            return "ok";
                        }),
                "java.lang.IllegalArgumentException: Attempt to write a read-only segment");

        // --- asSlice: rule, messages, order ---
        eq(
                "E1 asSlice(4,4,3)",
                attempt(() -> b16.asSlice(4, 4, 3).byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint : 3");
        eq(
                "E2 asSlice(4,4,0)",
                attempt(() -> b16.asSlice(4, 4, 0).byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint : 0");
        eq(
                "E3 asSlice(0,8,8)",
                attempt(() -> b16.asSlice(0, 8, 8).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E4 asSlice(4,4,1)", b16.asSlice(4, 4, 1).byteSize(), 4L);
        eq("E5 int[8] asSlice(0,4,4)", i8.asSlice(0, 4, 4).byteSize(), 4L);
        eq(
                "E6 int[8] asSlice(2,4,4)",
                attempt(() -> i8.asSlice(2, 4, 4).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq(
                "E7 int[8] asSlice(0,8,8)",
                attempt(() -> i8.asSlice(0, 8, 8).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        // Bounds beat BOTH alignment checks.
        eq("E8 asSlice(20,4,3) bounds first", exClass(() -> b16.asSlice(20, 4, 3)),
                "java.lang.IndexOutOfBoundsException");
        eq("E9 asSlice(17) class", exClass(() -> b16.asSlice(17)),
                "java.lang.IndexOutOfBoundsException");
        eq("E9b asSlice(4,-1) class", exClass(() -> b16.asSlice(4, -1)),
                "java.lang.IndexOutOfBoundsException");
        eq(
                "E10 asSlice(0,JAVA_INT)",
                attempt(() -> b16.asSlice(0, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq(
                "E11 asSlice(0,JAVA_INT_UNALIGNED)",
                b16.asSlice(0, ValueLayout.JAVA_INT_UNALIGNED).byteSize(),
                4L);
        eq(
                "E12 asSlice(0,paddingLayout(4))",
                b16.asSlice(0, MemoryLayout.paddingLayout(4)).byteSize(),
                4L);
        eq(
                "E13 asSlice(0,seq(2,JAVA_INT))",
                attempt(
                        () ->
                                b16.asSlice(0, MemoryLayout.sequenceLayout(2, ValueLayout.JAVA_INT))
                                        .byteSize()),
                "java.lang.IllegalArgumentException: Target offset incompatible with alignment constraints");
        eq("E14 int[8] asSlice(4,JAVA_INT)", i8.asSlice(4, ValueLayout.JAVA_INT).byteSize(), 4L);

        // --- the slice's start does not disqualify a better offset inside it ---
        eq("F1 int[8].asSlice(2).get(JAVA_INT,2)", i8.asSlice(2).get(ValueLayout.JAVA_INT, 2), 20);
        eq(
                "F2 int[8].asSlice(2).get(JAVA_INT,0)",
                exClass(() -> i8.asSlice(2).get(ValueLayout.JAVA_INT, 0)),
                "java.lang.IllegalArgumentException");
        eq("F3 int[8].asSlice(2).maxByteAlignment", i8.asSlice(2).maxByteAlignment(), 2L);
        eq(
                "F4 long[4].asSlice(4).get(JAVA_LONG,4)",
                MemorySegment.ofArray(new long[4]).asSlice(4).get(ValueLayout.JAVA_LONG, 4),
                0L);
        eq(
                "F5 long[4].asSlice(4).get(JAVA_LONG,0)",
                exClass(
                        () ->
                                MemorySegment.ofArray(new long[4])
                                        .asSlice(4)
                                        .get(ValueLayout.JAVA_LONG, 0)),
                "java.lang.IllegalArgumentException");

        // --- layouts: the JDK never pads, and the refusals it makes instead ---
        eq(
                "G1 struct(LONG,INT) byteSize",
                MemoryLayout.structLayout(ValueLayout.JAVA_LONG, ValueLayout.JAVA_INT).byteSize(),
                12L);
        eq(
                "G2 struct(INT,BYTE) byteSize",
                MemoryLayout.structLayout(ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE).byteSize(),
                5L);
        eq(
                "G3 struct(BYTE,INT)",
                attempt(
                        () ->
                                MemoryLayout.structLayout(ValueLayout.JAVA_BYTE, ValueLayout.JAVA_INT)
                                        .byteSize()),
                "java.lang.IllegalArgumentException: Invalid alignment constraint for member layout: i4");
        eq(
                "G4 struct(BYTE,pad(3),INT) byteSize",
                MemoryLayout.structLayout(
                                ValueLayout.JAVA_BYTE,
                                MemoryLayout.paddingLayout(3),
                                ValueLayout.JAVA_INT)
                        .byteSize(),
                8L);
        eq("G5 struct() byteAlignment", MemoryLayout.structLayout().byteAlignment(), 1L);
        eq(
                "G7 padding(0)",
                attempt(() -> MemoryLayout.paddingLayout(0).byteSize()),
                "java.lang.IllegalArgumentException: Invalid byte size: 0");
        eq(
                "G9 seq(-1,INT)",
                attempt(() -> MemoryLayout.sequenceLayout(-1, ValueLayout.JAVA_INT).byteSize()),
                "java.lang.IllegalArgumentException: The provided elementCount is negative: -1");
        eq(
                "G10 seq(MAX,INT)",
                attempt(
                        () ->
                                MemoryLayout.sequenceLayout(Long.MAX_VALUE, ValueLayout.JAVA_INT)
                                        .byteSize()),
                "java.lang.IllegalArgumentException: Layout size exceeds Long.MAX_VALUE");
        eq(
                "G12 seq(2,struct(INT,BYTE))",
                attempt(
                        () ->
                                MemoryLayout.sequenceLayout(
                                                2,
                                                MemoryLayout.structLayout(
                                                        ValueLayout.JAVA_INT, ValueLayout.JAVA_BYTE))
                                        .byteSize()),
                "java.lang.IllegalArgumentException: Element layout size is not multiple of alignment");
        eq(
                "G14 seq(3,struct()) elementCount",
                MemoryLayout.sequenceLayout(3, MemoryLayout.structLayout()).elementCount(),
                3L);
        eq(
                "G15 seq(3,struct()) byteSize",
                MemoryLayout.sequenceLayout(3, MemoryLayout.structLayout()).byteSize(),
                0L);
        eq(
                "G19 struct(INT,null)",
                attempt(() -> MemoryLayout.structLayout(ValueLayout.JAVA_INT, null).byteSize()),
                "java.lang.NullPointerException: null");
        eq(
                "G20 seq(4,null)",
                attempt(() -> MemoryLayout.sequenceLayout(4, null).byteSize()),
                "java.lang.NullPointerException: null");

        if (checks != EXPECTED_CHECKS) {
            throw new AssertionError(
                    "check count moved: expected " + EXPECTED_CHECKS + ", ran " + checks);
        }
        System.out.println("CK RJdkFfmSegment checks=" + checks);
        System.out.println("PASS RJdkFfmSegment (" + checks + " checks)");
    }
}
