// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// VoxelAlloc2 -- the kfusion per-voxel `Short2` allocation, reproduced without
// TornadoVM on the classpath.
//
// `docs/known-issues/jit/per-voxel-allocation-escapes-its-method-so-ea-cannot-help-20260827.md`
// measured this shape against `tornado-api-5.2.0-jdk25.jar`, which is a build
// artifact under `apps/kfusion-tornadovm/target/` and therefore not something a
// probe can depend on. The classes below reproduce the four-level accessor
// chain in the shapes that matter to the JIT:
//
//   VolumeShort2.get(x,y,z)              -> getIndex(III)I + loadFromArray
//   VolumeShort2.loadFromArray(a,i)      -> `new Short2()`, setX, setY, areturn
//   Short2.<init>()V                     -> `iconst_2; newarray short; <init>([S)V`
//   Short2.<init>([S)V                   -> putfield storage
//   Short2.setX/setY -> set(IS)V         -> `storage[i] = v`
//   ShortArr.get(I)S -> TSeg.getShortAtIndex(II)S -> MemorySegment.getAtIndex
//
// so `Short2` is TWO allocations (the object plus its `short[2]`) reached
// through a method that RETURNS it, backed by an FFM segment element read.
//
// Three arms, and the middle one is the CONTROL: `rawseg` performs the same two
// segment reads with no wrapper object, so the difference between it and
// `volume` is exactly what the allocation costs. `array` is the floor.
//
// Every arm computes the same checksum by construction, so a transform that
// breaks the read shows up as a wrong number rather than as a fast one.
//
// Each arm's hot loop lives in its own static method: a loop in `main` is
// OSR-only and never reaches the optimizing tier's admission gate.
//
//   cratonvm --java-home <jdk25> -cp probes/voxout VoxelAlloc2
//   CRATONVM_JIT_IR_INLINE=1 CRATONVM_DBG_SCALAR_NEW=1 ... VoxelAlloc2

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.util.Locale;

public final class VoxelAlloc2 {

    // -- uk.ac.manchester.tornado.api.types.arrays.TornadoMemorySegment --
    static final class TSeg {
        private final MemorySegment segment;

        TSeg(MemorySegment segment) {
            this.segment = segment;
        }

        public short getShortAtIndex(int index, int base) {
            return segment.getAtIndex(ValueLayout.JAVA_SHORT, base + index);
        }

        public void setShortAtIndex(int index, int base, short value) {
            segment.setAtIndex(ValueLayout.JAVA_SHORT, base + index, value);
        }
    }

    // -- uk.ac.manchester.tornado.api.types.arrays.ShortArray --
    static final class ShortArr {
        private final TSeg segment;
        private final int baseIndex;

        ShortArr(Arena arena, int numberOfElements) {
            this.segment = new TSeg(arena.allocate((long) numberOfElements * 2L, 8L));
            this.baseIndex = 0;
        }

        public short get(int index) {
            return segment.getShortAtIndex(index, baseIndex);
        }

        public void set(int index, short value) {
            segment.setShortAtIndex(index, baseIndex, value);
        }
    }

    // -- uk.ac.manchester.tornado.api.types.vectors.Short2 --
    static final class Short2 {
        private final short[] storage;

        private Short2(short[] storage) {
            this.storage = storage;
        }

        public Short2() {
            this(new short[2]);
        }

        public void setX(short value) {
            set(0, value);
        }

        public void setY(short value) {
            set(1, value);
        }

        private void set(int index, short value) {
            storage[index] = value;
        }

        public short getX() {
            return get(0);
        }

        public short getY() {
            return get(1);
        }

        private short get(int index) {
            return storage[index];
        }
    }

    // -- uk.ac.manchester.tornado.api.types.volumes.VolumeShort2 --
    static final class VolumeShort2 {
        private final ShortArr storage;
        private final int X;
        private final int Y;
        private final int Z;

        VolumeShort2(Arena arena, int x, int y, int z) {
            this.storage = new ShortArr(arena, x * y * z * 2);
            this.X = x;
            this.Y = y;
            this.Z = z;
        }

        public ShortArr getArray() {
            return storage;
        }

        private int getIndex(int x, int y, int z) {
            return z * X * Y * 2 + y * 2 * X + x * 2;
        }

        public Short2 get(int x, int y, int z) {
            int index = getIndex(x, y, z);
            return loadFromArray(storage, index);
        }

        private Short2 loadFromArray(ShortArr array, int index) {
            Short2 result = new Short2();
            result.setX(array.get(index));
            result.setY(array.get(index + 1));
            return result;
        }
    }

    // -- arms ---------------------------------------------------------------
    // `volume` is the target: two segment reads plus the `Short2` pair.
    static long sweepVolume(VolumeShort2 vol, int n) {
        long sum = 0;
        for (int z = 0; z < n; z++) {
            for (int y = 0; y < n; y++) {
                for (int x = 0; x < n; x++) {
                    Short2 v = vol.get(x, y, z);
                    sum += v.getX() + v.getY();
                }
            }
        }
        return sum;
    }

    // `rawseg` is the CONTROL: the same two reads, no wrapper object.
    static long sweepRawSeg(ShortArr arr, int n) {
        long sum = 0;
        for (int z = 0; z < n; z++) {
            for (int y = 0; y < n; y++) {
                for (int x = 0; x < n; x++) {
                    int i = z * n * n * 2 + y * 2 * n + x * 2;
                    sum += arr.get(i) + arr.get(i + 1);
                }
            }
        }
        return sum;
    }

    // `array` is the floor: a plain `short[]`.
    static long sweepArray(short[] a, int n) {
        long sum = 0;
        for (int z = 0; z < n; z++) {
            for (int y = 0; y < n; y++) {
                for (int x = 0; x < n; x++) {
                    int i = z * n * n * 2 + y * 2 * n + x * 2;
                    sum += a[i] + a[i + 1];
                }
            }
        }
        return sum;
    }

    private static short fill(int i) {
        return (short) ((i * 2654435761L) >>> 17 & 0x3FF);
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("voxel.n", 64);
        int reps = Integer.getInteger("voxel.reps", 5);
        int warmN = Integer.getInteger("voxel.warm", 8);
        int warmIters = Integer.getInteger("voxel.warmiters", 300);

        try (Arena arena = Arena.ofConfined()) {
            // Warm on a small volume so compilation is not inside the timed window.
            VolumeShort2 warmVol = new VolumeShort2(arena, warmN, warmN, warmN);
            ShortArr warmArr = warmVol.getArray();
            short[] warmHeap = new short[warmN * warmN * warmN * 2];
            for (int i = 0; i < warmN * warmN * warmN * 2; i++) {
                warmArr.set(i, fill(i));
                warmHeap[i] = fill(i);
            }
            long warmBlackhole = 0;
            for (int r = 0; r < warmIters; r++) {
                warmBlackhole += sweepVolume(warmVol, warmN);
                warmBlackhole += sweepRawSeg(warmArr, warmN);
                warmBlackhole += sweepArray(warmHeap, warmN);
            }

            VolumeShort2 vol = new VolumeShort2(arena, n, n, n);
            ShortArr arr = vol.getArray();
            short[] heap = new short[n * n * n * 2];
            for (int i = 0; i < n * n * n * 2; i++) {
                arr.set(i, fill(i));
                heap[i] = fill(i);
            }

            long voxels = (long) n * n * n;
            System.out.println("n=" + n + " voxels=" + voxels + " warm=" + warmN
                    + " warmBlackhole=" + (warmBlackhole != 0));
            for (int r = 0; r < reps; r++) {
                long t0 = System.nanoTime();
                long a = sweepVolume(vol, n);
                long t1 = System.nanoTime();
                long b = sweepRawSeg(arr, n);
                long t2 = System.nanoTime();
                long c = sweepArray(heap, n);
                long t3 = System.nanoTime();
                System.out.printf(
                        Locale.ROOT,
                        "rep=%d volume=%.1f ns/voxel rawseg=%.1f ns/voxel array=%.1f ns/voxel"
                                + "  sums=%d/%d/%d%n",
                        r, (t1 - t0) / (double) voxels, (t2 - t1) / (double) voxels,
                        (t3 - t2) / (double) voxels, a, b, c);
                if (a != b || b != c) {
                    System.out.println("*** CHECKSUM MISMATCH -- an arm read the wrong data");
                }
            }
        }
    }
}
