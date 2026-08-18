// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// What a fast-path arm could be worth, per opcode (interpreter audit, phase 2).
//
// `getfield`, `putfield`, `getstatic`, `putstatic`, `ldc`, `new`, `checkcast`,
// `instanceof`, `tableswitch`, `lookupswitch`, `monitorenter` and
// `monitorexit` have no arm in the raw-bytecode fast path of
// `execute_frame_from_index`. Each therefore pays, on EVERY execution: the
// quickened stream's `resolve(pc)`, the frame-pointer hoist and its
// `code_ptr`/`quick_code_ptr` compare, a non-inlined call into
// `execute_instruction`, the match over a ~200-variant `Instruction`, and the
// post-call diagnostic and error-conversion checks — before the arm body runs.
//
// Adding an arm removes that fixed overhead and nothing else. So the question
// this probe answers is NOT "how expensive is getfield" but "what fraction of
// getfield is the part an arm can remove", and that needs two numbers:
//
//   1. the fixed decoded-dispatch overhead, in ns per opcode, and
//   2. the marginal cost of each candidate opcode as it stands.
//
// Both come out of differences between loops that are identical except for a
// known count of one opcode. `baseKernel` is the shared shape; each `*Kernel`
// adds exactly UNROLL copies of one opcode to it. `nopKernel` adds UNROLL
// copies of an opcode that ALREADY has a fast-path arm (`iadd`), which is the
// control: subtract it and what is left is the candidate's cost over an
// already-fast-pathed operation of similar shape.
//
// Run under `--nojit`. With the JIT on these loops tier up and the
// interpreter's per-opcode cost stops being what is measured.
public final class InterpDecodedOpcodeCostProbe {

    static final int UNROLL = 16;

    static final class Holder {
        int a, b, c, d;
        Object o;
        long l;
    }

    static int sField;
    static Object sObj = new Holder();

    static final Holder H = new Holder();
    static final Object OBJ = new Holder();

    // Shared shape: an int accumulator loop with no candidate opcode in it.
    private static long baseKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
        }
        return acc;
    }

    // CONTROL: + UNROLL x `iadd`, an opcode that already has a fast-path arm.
    private static long nopKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            acc += 1; acc += 1; acc += 1; acc += 1;
            acc += 1; acc += 1; acc += 1; acc += 1;
            acc += 1; acc += 1; acc += 1; acc += 1;
            acc += 1; acc += 1; acc += 1; acc += 1;
        }
        return acc;
    }

    // + UNROLL x `getfield`.
    private static long getfieldKernel(int iters) {
        int acc = 0;
        Holder h = H;
        for (int i = 0; i < iters; i++) {
            acc += i;
            acc += h.a; acc += h.a; acc += h.a; acc += h.a;
            acc += h.a; acc += h.a; acc += h.a; acc += h.a;
            acc += h.a; acc += h.a; acc += h.a; acc += h.a;
            acc += h.a; acc += h.a; acc += h.a; acc += h.a;
        }
        return acc;
    }

    // + UNROLL x `putfield` (the getfield above is the read side; this arm's
    // extra cost over `getfieldKernel` is the write barrier plus the store).
    private static long putfieldKernel(int iters) {
        int acc = 0;
        Holder h = H;
        for (int i = 0; i < iters; i++) {
            acc += i;
            h.b = i; h.b = i; h.b = i; h.b = i;
            h.b = i; h.b = i; h.b = i; h.b = i;
            h.b = i; h.b = i; h.b = i; h.b = i;
            h.b = i; h.b = i; h.b = i; h.b = i;
        }
        return acc + h.b;
    }

    // + UNROLL x `getstatic`.
    private static long getstaticKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            acc += sField; acc += sField; acc += sField; acc += sField;
            acc += sField; acc += sField; acc += sField; acc += sField;
            acc += sField; acc += sField; acc += sField; acc += sField;
            acc += sField; acc += sField; acc += sField; acc += sField;
        }
        return acc;
    }

    // + UNROLL x `putstatic`.
    private static long putstaticKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            sField = i; sField = i; sField = i; sField = i;
            sField = i; sField = i; sField = i; sField = i;
            sField = i; sField = i; sField = i; sField = i;
            sField = i; sField = i; sField = i; sField = i;
        }
        return acc + sField;
    }

    // + UNROLL x `ldc` (an int constant too large for sipush).
    private static long ldcKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            acc += 1000000; acc += 1000001; acc += 1000002; acc += 1000003;
            acc += 1000004; acc += 1000005; acc += 1000006; acc += 1000007;
            acc += 1000008; acc += 1000009; acc += 1000010; acc += 1000011;
            acc += 1000012; acc += 1000013; acc += 1000014; acc += 1000015;
        }
        return acc;
    }

    // + UNROLL x `checkcast`.
    //
    // CORRECTED 2026-08-18: this kernel used to read a field through the cast
    // (`((Holder) o).a`), so its marginal cost was checkcast PLUS a getfield —
    // and getfield is the most expensive opcode in the table. Reported as
    // "checkcast", that inflated it by ~180 ns and made checkcast look 1.7x
    // instanceof when the two are within noise of each other. The confound was
    // noted in this file's own comments and then not applied when the summary
    // table was read, which is the more useful half of the lesson: an
    // instrument's caveat has to live in the OUTPUT, not in the source.
    //
    // The cast result is now consumed by an `if_acmpeq` against the same object
    // — an opcode with a fast-path arm costing ~10 ns — so the row is
    // checkcast plus a rounding error.
    private static long checkcastKernel(int iters) {
        int acc = 0;
        Object o = OBJ;
        for (int i = 0; i < iters; i++) {
            acc += i;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
            if (((Holder) o) == o) acc++; if (((Holder) o) == o) acc++;
        }
        return acc;
    }

    // + UNROLL x `instanceof`.
    private static long instanceofKernel(int iters) {
        int acc = 0;
        Object o = OBJ;
        for (int i = 0; i < iters; i++) {
            acc += i;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
            if (o instanceof Holder) acc++; if (o instanceof Holder) acc++;
        }
        return acc;
    }

    // + UNROLL x `monitorenter`/`monitorexit` pairs (uncontended, one thread).
    private static long monitorKernel(int iters) {
        int acc = 0;
        Object lock = OBJ;
        for (int i = 0; i < iters; i++) {
            acc += i;
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
            synchronized (lock) { acc++; } synchronized (lock) { acc++; }
        }
        return acc;
    }

    // + UNROLL x `tableswitch` (dense cases -> tableswitch).
    private static long tableswitchKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            int k = i & 7;
            for (int u = 0; u < UNROLL; u++) {
                switch (k) {
                    case 0: acc += 1; break;
                    case 1: acc += 2; break;
                    case 2: acc += 3; break;
                    case 3: acc += 4; break;
                    case 4: acc += 5; break;
                    case 5: acc += 6; break;
                    case 6: acc += 7; break;
                    default: acc += 8; break;
                }
            }
        }
        return acc;
    }

    // The tableswitch kernel's inner `for` is itself extra work; this is its
    // control -- the same inner loop with the switch replaced by an iadd.
    private static long tableswitchControlKernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += i;
            int k = i & 7;
            for (int u = 0; u < UNROLL; u++) {
                acc += k;
            }
        }
        return acc;
    }

    private static long time(java.util.function.IntToLongFunction f, int iters, long[] sink) {
        long t0 = System.nanoTime();
        sink[0] += f.applyAsLong(iters);
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 8;

        String[] names = {
            "base", "nop(iadd)", "getfield", "putfield", "getstatic", "putstatic",
            "ldc", "checkcast", "instanceof", "monitor(enter+exit)",
            "tableswitch", "tableswitchControl",
        };
        long[] tot = new long[names.length];
        long[] sink = new long[1];

        // Interleave every kernel within each rep so drift hits them equally.
        for (int r = 0; r < reps; r++) {
            tot[0] += time(InterpDecodedOpcodeCostProbe::baseKernel, iters, sink);
            tot[1] += time(InterpDecodedOpcodeCostProbe::nopKernel, iters, sink);
            tot[2] += time(InterpDecodedOpcodeCostProbe::getfieldKernel, iters, sink);
            tot[3] += time(InterpDecodedOpcodeCostProbe::putfieldKernel, iters, sink);
            tot[4] += time(InterpDecodedOpcodeCostProbe::getstaticKernel, iters, sink);
            tot[5] += time(InterpDecodedOpcodeCostProbe::putstaticKernel, iters, sink);
            tot[6] += time(InterpDecodedOpcodeCostProbe::ldcKernel, iters, sink);
            tot[7] += time(InterpDecodedOpcodeCostProbe::checkcastKernel, iters, sink);
            tot[8] += time(InterpDecodedOpcodeCostProbe::instanceofKernel, iters, sink);
            tot[9] += time(InterpDecodedOpcodeCostProbe::monitorKernel, iters, sink);
            tot[10] += time(InterpDecodedOpcodeCostProbe::tableswitchKernel, iters, sink);
            tot[11] += time(InterpDecodedOpcodeCostProbe::tableswitchControlKernel, iters, sink);
        }

        long opcodes = (long) iters * reps * UNROLL;
        System.out.println("sink=" + sink[0] + " opcode_executions_per_row=" + opcodes);
        for (int i = 0; i < names.length; i++) {
            // Marginal ns per added opcode, over the shared base shape.
            long baseline = (i == 11 || i == 10) ? tot[11] : tot[0];
            double marginal = (double) (tot[i] - baseline) / (double) opcodes;
            System.out.printf("%-20s total_ms=%-7d marginal_ns_per_op=%.1f%n",
                    names[i], tot[i] / 1_000_000, marginal);
        }
    }
}
