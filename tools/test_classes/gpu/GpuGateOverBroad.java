// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// What does `--gpu` cost a program that never offloads anything?
//
// `offload_jit_gate` denies JIT admission for two reasons, and until
// 2026-09-04 both were far wider than the thing they protected:
//
//  1. WRITES-PRIMITIVE-ARRAY. Any method containing `iastore` and
//     friends, because a compiled inline store could not evict the GPU
//     input-residency cache. `Vec3.set` below is that shape -- a
//     two-line accessor over a `float[]` field. On kfusion this reason
//     alone denied compilation to 21 methods: the whole TornadoVM
//     `Float2/3/4/8`, `Int2/3`, `Short2`, `Byte3/4` and `ImageFloat`
//     accessor family, which is what its inner loops are built from.
//
//  2. CALLS-ELIGIBLE-KERNEL. Any method containing an `invokestatic`
//     whose target the GPU analyzer calls `Eligible`. `clamp` below is
//     that shape, and its targets are `java/lang/Math.min(II)I` and
//     `Math.max(II)I` -- which ARE `Eligible`, because eligibility is a
//     statement about a method's bytecode, and which the DISPATCHER can
//     never launch, because it only transparently dispatches a `)V` map
//     or a proven `)I`/`)J` reduction over an array argument.
//
// Neither reason is reached by anything in this file that could ever
// run on a device. There is no kernel here and no array big enough to
// clear `--gpu-min-work`; the whole point is that `--gpu` used to
// de-optimise a program like this anyway.
//
// Read the verdict, not the time: `bench-gpu/gate-overbroad.sh` runs
// this and diffs the `[cratonvm] gpu jit gate:` census across the two
// kill switches. A wall-clock number on a desktop would be noise at
// this size, and the census is the fact under test.
//
// Usage: java GpuGateOverBroad [rounds]
public class GpuGateOverBroad {

    /// A TornadoVM-style vector: a primitive array behind an accessor.
    static final class Vec3 {
        final float[] storage = new float[3];

        // `fastore` in a method that does nothing else -- reason 1.
        void set(int i, float v) {
            storage[i] = v;
        }

        float get(int i) {
            return storage[i];
        }
    }

    static final class Img {
        final float[] storage;
        final int w;

        Img(int w, int h) {
            this.w = w;
            this.storage = new float[w * h];
        }

        void set(int x, int y, float v) {
            storage[x + y * w] = v;
        }

        float get(int x, int y) {
            return storage[x + y * w];
        }
    }

    // Two `invokestatic`s to `Math`, and nothing else -- reason 2.
    // `Math.min(II)I` and `Math.max(II)I` are both analyzer-`Eligible`.
    static int clamp(int v, int lo, int hi) {
        return Math.max(lo, Math.min(hi, v));
    }

    // A caller of `clamp`, to show the reason travels: blocking `clamp`
    // does not block this one directly, but leaving `clamp` interpreted
    // puts a frame on every iteration of whatever calls it.
    static int band(int v) {
        return clamp(v, 0, 255) + clamp(v >> 1, 0, 127);
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        Vec3 v = new Vec3();
        Img img = new Img(64, 64);
        double acc = 0;
        long ints = 0;

        for (int r = 0; r < rounds; r++) {
            v.set(0, r * 0.5f);
            v.set(1, r * 0.25f);
            v.set(2, r * 0.125f);

            int y = r & 63;
            img.set(r & 63, y, v.get(0) + y);
            acc += img.get(r & 63, y) + v.get(1) + v.get(2);

            ints += band(r);
        }

        System.out.println("gate_acc=" + (long) acc);
        System.out.println("gate_ints=" + ints);
    }
}
