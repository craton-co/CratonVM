// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.ByteOrder;
import jdk.incubator.vector.FloatVector;
import jdk.incubator.vector.IntVector;
import jdk.incubator.vector.ShortVector;
import jdk.incubator.vector.VectorOperators;
import jdk.incubator.vector.VectorSpecies;

/**
 * Prices GPULlama3's inference kernel, so its cost can be stated in seconds
 * rather than in "still running".
 *
 * `FP16FloatTensor.vectorDot` is what `FloatTensor.matmul` spends every token
 * in: read FP16 halves out of a `MemorySegment` with
 * `ShortVector.fromMemorySegment`, widen to int lanes, rebuild IEEE-754
 * binary32 by hand, multiply into a float accumulator. A Llama-3.2-1B forward
 * pass is on the order of 1.2e9 of these multiply-adds.
 *
 * This is a THROUGHPUT probe, not a correctness one -- `FfmVectorSegmentProbe`
 * owns correctness and checks every lane against the oracle. The checksum here
 * exists only so the loop cannot be optimised away, and is printed so a run
 * that computed something different is not silently compared.
 *
 *   Fp16VectorDotBench [dots] [lengthPerDot]
 *
 * Run with `--add-modules jdk.incubator.vector --enable-native-access=ALL-UNNAMED`.
 * The interesting number is ns_per_lane: it divides out the loop shape, so a
 * CratonVM run and a HotSpot run of different sizes still compare.
 */
public class Fp16VectorDotBench {

    static final VectorSpecies<Short> S_SPECIES_HALF = ShortVector.SPECIES_128;
    static final VectorSpecies<Integer> I_SPECIES = IntVector.SPECIES_256;
    static final VectorSpecies<Float> F_SPECIES = FloatVector.SPECIES_256;

    public static void main(String[] args) {
        int dots = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        int len = args.length > 1 ? Integer.parseInt(args[1]) : 2048;
        len -= len % F_SPECIES.length();

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(len * 2L, 8);
            for (int i = 0; i < len; i++) {
                seg.set(ValueLayout.JAVA_SHORT, i * 2L, (short) (0x3C00 + (i * 37 & 0x03FF)));
            }
            float[] other = new float[len];
            for (int i = 0; i < len; i++) {
                other[i] = ((i % 7) - 3) * 0.25f;
            }

            // One untimed pass so a tiered compiler has seen the loop, and so
            // the first timed pass is not paying class init for the Vector API.
            double warm = dot(seg, other, len);

            double checksum = 0;
            long best = Long.MAX_VALUE;
            for (int pass = 0; pass < 3; pass++) {
                long t0 = System.nanoTime();
                double acc = 0;
                for (int d = 0; d < dots; d++) {
                    acc += dot(seg, other, len);
                }
                long dt = System.nanoTime() - t0;
                if (dt < best) {
                    best = dt;
                }
                checksum = acc;
            }
            long lanes = (long) dots * len;
            System.out.println("FP16DOTBENCH dots=" + dots + " len=" + len
                    + " lanes=" + lanes
                    + " best_ms=" + (best / 1e6)
                    + " ns_per_lane=" + (best / (double) lanes)
                    + " checksum=" + Float.floatToRawIntBits((float) checksum)
                    + " warm=" + Float.floatToRawIntBits((float) warm));
        }
    }

    /** `FP16FloatTensor.vectorDot`, reduced to one segment and one float[]. */
    static float dot(MemorySegment seg, float[] other, int len) {
        FloatVector acc = FloatVector.zero(F_SPECIES);
        int bound = F_SPECIES.loopBound(len);
        for (int i = 0; i < bound; i += F_SPECIES.length()) {
            FloatVector b = FloatVector.fromArray(F_SPECIES, other, i);
            ShortVector sv = ShortVector.fromMemorySegment(
                    S_SPECIES_HALF, seg, i * 2L, ByteOrder.LITTLE_ENDIAN);
            IntVector bits = sv.castShape(I_SPECIES, 0).reinterpretAsInts();
            IntVector isNonZeroExp = bits.and(0x7C00).neg()
                    .lanewise(VectorOperators.ASHR, 31);
            IntVector assembled = bits.and(0x8000).lanewise(VectorOperators.LSHL, 16)
                    .or(bits.and(0x7FFF).add(0x1C000)
                            .lanewise(VectorOperators.LSHL, 13)
                            .and(isNonZeroExp));
            acc = assembled.reinterpretAsFloats().fma(b, acc);
        }
        return acc.reduceLanes(VectorOperators.ADD);
    }
}
