// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import jdk.incubator.vector.FloatVector;
import jdk.incubator.vector.IntVector;
import jdk.incubator.vector.ShortVector;
import jdk.incubator.vector.Vector;
import jdk.incubator.vector.VectorOperators;
import jdk.incubator.vector.VectorSpecies;

/**
 * The Vector-API-over-MemorySegment path, in the shape GPULlama3 uses it.
 *
 * `bug-gpullama3-model-load-and-ffm-segment-class-identity.md` died on the
 * first `matmul` with
 *
 *   ClassCastException: class java.lang.foreign.MemorySegment cannot be cast
 *     to class jdk.internal.foreign.AbstractMemorySegmentImpl
 *
 * because CratonVM stamped every segment it minted with the INTERFACE. The
 * JDK's own consumers cast a segment down to its abstract base; the Vector
 * API does it on every segment entry point, and `AbstractVector
 * .defaultReinterpret` reaches one for every `reinterpretAsInts()` -- it
 * round-trips the vector through a scratch `MemorySegment.ofArray(new
 * byte[n])`.
 *
 * Sections, each independently wrapped so one failure cannot hide the ones
 * behind it:
 *
 *   IDENTITY  what a minted segment's class IS, and what it is assignable to.
 *             This is the row that cannot match HotSpot -- CratonVM has no
 *             `NativeMemorySegmentImpl` -- so it prints the ASSIGNABILITY
 *             answers, which must match, and the class name only as a note.
 *   LOAD      `ShortVector.fromMemorySegment` off an arena segment.
 *   STORE     `IntVector.intoMemorySegment` into a heap segment.
 *   REINTERP  `castShape` + `reinterpretAsInts`, i.e. `defaultReinterpret`,
 *             which is the exact frame the app died in.
 *   FP16DOT   the whole `FP16FloatTensor.vectorDot` kernel, reduced to one
 *             checksum. A wrong lane, a wrong byte order or a wrong offset
 *             all move it.
 *   ALIGN     `ValueLayout.JAVA_INT.withByteAlignment(1)` -- the layout
 *             `IntVector.<clinit>` builds. Nine registrations of
 *             `withByteAlignment` returned the receiver unchanged, so this
 *             asked a `byte[]`-backed segment for a 4-byte-aligned write and
 *             was correctly refused.
 *
 * Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.
 * Every printed line is deterministic, so CratonVM and HotSpot diff exactly.
 */
public class FfmVectorSegmentProbe {

    static int sections = 0, failed = 0;

    static void section(String name, Runnable body) {
        sections++;
        try {
            body.run();
        } catch (Throwable t) {
            failed++;
            System.out.println("SECTION-FAILED " + name + ": " + t);
        }
    }

    public static void main(String[] args) {
        section("IDENTITY", FfmVectorSegmentProbe::identity);
        section("LOAD", FfmVectorSegmentProbe::load);
        section("STORE", FfmVectorSegmentProbe::store);
        section("REINTERP", FfmVectorSegmentProbe::reinterp);
        section("FP16DOT", FfmVectorSegmentProbe::fp16dot);
        section("ALIGN", FfmVectorSegmentProbe::align);
        System.out.println("SECTIONS " + sections + " FAILED " + failed);
        System.out.println(failed == 0 ? "PASS FfmVectorSegmentProbe" : "FAIL FfmVectorSegmentProbe");
    }

    /** What a minted segment is, and what it is assignable to. */
    static void identity() {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment nativeSeg = arena.allocate(64, 8);
            MemorySegment heapSeg = MemorySegment.ofArray(new byte[64]);
            MemorySegment slice = nativeSeg.asSlice(8, 16);
            for (Object[] row : new Object[][] {
                    { "arena", nativeSeg }, { "ofArray", heapSeg }, { "asSlice", slice } }) {
                MemorySegment s = (MemorySegment) row[1];
                Class<?> c = s.getClass();
                // The one line that cannot match HotSpot, and says so.
                System.out.println("IDENTITY-NOTE " + row[0] + " class=" + c.getName());
                // These must match, and are what the JDK's internals rely on.
                System.out.println("IDENTITY " + row[0]
                        + " isInterface=" + c.isInterface()
                        + " isMemorySegment=" + (s instanceof MemorySegment)
                        + " abstractBase=" + isAbstractSegmentImpl(s));
            }
        }
    }

    /**
     * `s instanceof jdk.internal.foreign.AbstractMemorySegmentImpl`, without
     * naming a non-exported class at compile time. The Vector API's
     * `checkcast` asks exactly this question; asking it reflectively here
     * keeps the probe compilable on a stock JDK with no `--add-exports`.
     */
    static boolean isAbstractSegmentImpl(MemorySegment s) {
        try {
            Class<?> impl = Class.forName("jdk.internal.foreign.AbstractMemorySegmentImpl");
            return impl.isInstance(s);
        } catch (ClassNotFoundException e) {
            return false;
        }
    }

    static void load() {
        VectorSpecies<Short> species = ShortVector.SPECIES_128;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(64, 8);
            for (int i = 0; i < 32; i++) {
                seg.set(ValueLayout.JAVA_SHORT, i * 2L, (short) (i * 7 - 11));
            }
            ShortVector v = ShortVector.fromMemorySegment(species, seg, 0, ByteOrder.LITTLE_ENDIAN);
            StringBuilder sb = new StringBuilder("LOAD lanes");
            for (int i = 0; i < species.length(); i++) {
                sb.append(' ').append(v.lane(i));
            }
            System.out.println(sb);
        }
    }

    static void store() {
        VectorSpecies<Integer> species = IntVector.SPECIES_128;
        byte[] backing = new byte[64];
        MemorySegment seg = MemorySegment.ofArray(backing);
        int[] src = new int[species.length()];
        for (int i = 0; i < src.length; i++) {
            src[i] = 0x01020304 + i;
        }
        IntVector.fromArray(species, src, 0).intoMemorySegment(seg, 0, ByteOrder.LITTLE_ENDIAN);
        StringBuilder sb = new StringBuilder("STORE bytes");
        for (int i = 0; i < species.length() * 4; i++) {
            sb.append(' ').append(backing[i] & 0xFF);
        }
        System.out.println(sb);
        // And the round trip, which is what `defaultReinterpret` does.
        IntVector back = IntVector.fromMemorySegment(species, seg, 0, ByteOrder.LITTLE_ENDIAN);
        StringBuilder rb = new StringBuilder("STORE roundtrip");
        for (int i = 0; i < species.length(); i++) {
            rb.append(' ').append(back.lane(i));
        }
        System.out.println(rb);
    }

    /** `castShape` + `reinterpretAsInts` — `AbstractVector.defaultReinterpret`. */
    static void reinterp() {
        VectorSpecies<Short> shortSpecies = ShortVector.SPECIES_64;
        VectorSpecies<Integer> intSpecies = IntVector.SPECIES_128;
        short[] src = { 1, -2, 3, -4 };
        ShortVector sv = ShortVector.fromArray(shortSpecies, src, 0);
        Vector<Integer> widened = sv.castShape(intSpecies, 0);
        IntVector iv = widened.reinterpretAsInts();
        StringBuilder sb = new StringBuilder("REINTERP lanes");
        for (int i = 0; i < intSpecies.length(); i++) {
            sb.append(' ').append(iv.lane(i));
        }
        System.out.println(sb);
    }

    /**
     * `FP16FloatTensor.vectorDot`, verbatim in shape: read FP16 shorts out of
     * a segment, widen to int lanes, rebuild IEEE-754 binary32 by hand, and
     * accumulate against a float vector.
     */
    static void fp16dot() {
        VectorSpecies<Short> shortHalf = ShortVector.SPECIES_128;
        VectorSpecies<Integer> intSpecies = IntVector.SPECIES_256;
        VectorSpecies<Float> floatSpecies = FloatVector.SPECIES_256;
        int lanes = floatSpecies.length();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(lanes * 2L, 8);
            // A spread of halves: sign bits, denormals excluded, a zero.
            for (int i = 0; i < lanes; i++) {
                seg.set(ValueLayout.JAVA_SHORT, i * 2L, (short) (0x3C00 + i * 0x0180 - (i % 3) * 0x8000));
            }
            float[] other = new float[lanes];
            for (int i = 0; i < lanes; i++) {
                other[i] = (i % 5) - 2.0f;
            }
            FloatVector acc = FloatVector.zero(floatSpecies);
            ShortVector sv = ShortVector.fromMemorySegment(shortHalf, seg, 0, ByteOrder.LITTLE_ENDIAN);
            IntVector bits = sv.castShape(intSpecies, 0).reinterpretAsInts();
            IntVector expMask = bits.and(0x7C00);
            IntVector isNonZeroExp = expMask.neg().lanewise(VectorOperators.ASHR, 31);
            IntVector assembled = bits.and(0x8000).lanewise(VectorOperators.LSHL, 16)
                    .or(bits.and(0x7FFF).add(0x1C000).lanewise(VectorOperators.LSHL, 13)
                            .and(isNonZeroExp));
            FloatVector halves = assembled.reinterpretAsFloats();
            acc = acc.add(halves.mul(FloatVector.fromArray(floatSpecies, other, 0)));
            StringBuilder sb = new StringBuilder("FP16DOT lanes");
            for (int i = 0; i < lanes; i++) {
                sb.append(' ').append(Float.floatToRawIntBits(acc.lane(i)));
            }
            System.out.println(sb);
            System.out.println("FP16DOT sum " + Float.floatToRawIntBits(acc.reduceLanes(VectorOperators.ADD)));
        }
    }

    /**
     * `withByteAlignment` — the layout every Vector API segment store uses.
     * A no-op implementation shows up here as a refused write, and as
     * `byteAlignment` reporting the ORIGINAL value.
     */
    static void align() {
        ValueLayout.OfInt strict = ValueLayout.JAVA_INT;
        ValueLayout.OfInt loose = ValueLayout.JAVA_INT.withByteAlignment(1);
        System.out.println("ALIGN strict=" + strict.byteAlignment()
                + " loose=" + loose.byteAlignment()
                + " looseSize=" + loose.byteSize()
                + " strictUnchanged=" + strict.byteAlignment());
        byte[] backing = new byte[16];
        MemorySegment seg = MemorySegment.ofArray(backing);
        // A byte[]-backed segment caps alignment at 1, so this write is legal
        // ONLY with the relaxed layout — which is precisely why the Vector API
        // builds one.
        seg.set(loose, 0, 0x01020304);
        System.out.println("ALIGN wrote " + (backing[0] & 0xFF) + " " + (backing[1] & 0xFF)
                + " " + (backing[2] & 0xFF) + " " + (backing[3] & 0xFF));
        System.out.println("ALIGN readback " + seg.get(loose, 0));
        String strictRefusal;
        try {
            seg.set(strict, 0, 0x05060708);
            strictRefusal = "accepted";
        } catch (IllegalArgumentException e) {
            strictRefusal = "IllegalArgumentException";
        }
        System.out.println("ALIGN strictWrite " + strictRefusal);
        // A non-power-of-two alignment is an IllegalArgumentException on both.
        String bad;
        try {
            ValueLayout.JAVA_INT.withByteAlignment(3);
            bad = "accepted";
        } catch (IllegalArgumentException e) {
            bad = "IllegalArgumentException";
        }
        System.out.println("ALIGN badAlignment " + bad);
    }
}
