// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter fast-path COVERAGE probe (interpreter audit 2026-08-18).
//
// The raw-bytecode fast path in `execute_frame_from_index` had arms for
// `ishl`/`ishr`/`iushr` but not `lshl`/`lshr`/`lushr`, for `ineg`/`lneg` but
// not `fneg`/`dneg`, for `lcmp` but not `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`, and
// for `i2l`/`i2f`/`i2d`/`l2i` but not the eight remaining conversions. An
// opcode with no arm falls through to the decoded handler: resolve the pc in
// the quickened stream, enter a ~200-arm `Instruction` match, and re-index
// `thread.frames[frame_idx]` once per operand touched.
//
// This probe carries its own control. Both kernels do the same shape of work
// — a tight arithmetic loop over the same iteration count — but:
//
//   * `controlKernel` uses ONLY opcodes that already had fast-path arms
//     (`iadd`, `imul`, `ishl`, `iushr`, `ixor`, `lcmp`, `i2l`). Its number must
//     NOT move between the two binaries. If it does, something other than
//     opcode coverage changed and the other arm's delta is not attributable.
//
//   * `coverageKernel` is dominated by opcodes that had NO arm (`lshl`,
//     `lushr`, `lxor` is fine, `dcmpl`, `f2d`, `d2l`, `dneg`). Its number is
//     what the added arms are supposed to move.
//
// Run under `--nojit`: with the JIT on, both kernels tier up and the
// interpreter's per-opcode cost stops being what is measured.
public final class InterpFastPathCoverageProbe {

    // CONTROL — every opcode here already had a fast-path arm.
    private static int controlKernel(int iters, int seed) {
        int h = seed;
        for (int i = 0; i < iters; i++) {
            h += i;
            h *= 31;
            h ^= h >>> 15;
            h = h << 3 | h >>> 29;
            h ^= i;
        }
        return h;
    }

    // ARM UNDER TEST — long shifts, double compare, dneg and three of the
    // conversions that had no arm.
    private static long coverageKernel(int iters, long seed) {
        long h = seed;
        double acc = 0.0d;
        for (int i = 0; i < iters; i++) {
            h ^= h >>> 33;          // lushr
            h *= 0xFF51AFD7ED558CCDL;
            h ^= h << 17;           // lshl
            h ^= h >> 7;            // lshr
            float f = (float) i;    // i2f (already had an arm)
            double d = f;           // f2d
            acc += -d;              // dneg
            if (acc < 0.0d) {       // dcmpg
                acc = -acc;         // dneg
            }
            h += (long) acc;        // d2l
        }
        return h + (long) acc;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 12;

        long tc = 0, tk = 0;
        int hc = 0;
        long hk = 0;
        // Interleave so a host hiccup cannot land on one arm only.
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            hc += controlKernel(iters, r);
            long t1 = System.nanoTime();
            hk += coverageKernel(iters, r);
            long t2 = System.nanoTime();
            tc += t1 - t0;
            tk += t2 - t1;
        }
        System.out.println("checksum control=" + hc + " coverage=" + hk);
        System.out.println("controlKernel(pre-existing arms) total_ms=" + (tc / 1_000_000));
        System.out.println("coverageKernel(new arms)         total_ms=" + (tk / 1_000_000));
    }
}
