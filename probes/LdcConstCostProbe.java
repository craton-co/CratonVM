// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Marginal cost of one `ldc`, by tag, against an `iadd` control.
//
// Each kernel runs UNROLL copies of ONE opcode per loop iteration and is
// differenced against an otherwise identical loop with UNROLL fewer of them,
// so what is reported is the marginal cost of the opcode and not of the loop.
// The `iadd` control must NOT move between arms; if it does, the host was
// loaded and the run is not comparable.
//
// Consumption matters. An earlier probe in this tree consumed a `checkcast`
// by reading a field THROUGH it, so it reported checkcast+getfield as
// "checkcast" and inflated it by ~180ns. Every kernel here consumes its value
// with a reference or int comparison (~10ns, itself fast-pathed) and nothing
// else.
public final class LdcConstCostProbe {

    static final int UNROLL = 16;

    // ldc of a CONSTANT_String: the arm that allocated an owned Rust String
    // and hashed its content through the intern pool on EVERY execution.
    static int ldcStringKernel(int iters, Object sink) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            if ("ldc-cost-probe-literal-0" != sink) acc++;
            if ("ldc-cost-probe-literal-1" != sink) acc++;
            if ("ldc-cost-probe-literal-2" != sink) acc++;
            if ("ldc-cost-probe-literal-3" != sink) acc++;
            if ("ldc-cost-probe-literal-4" != sink) acc++;
            if ("ldc-cost-probe-literal-5" != sink) acc++;
            if ("ldc-cost-probe-literal-6" != sink) acc++;
            if ("ldc-cost-probe-literal-7" != sink) acc++;
            if ("ldc-cost-probe-literal-8" != sink) acc++;
            if ("ldc-cost-probe-literal-9" != sink) acc++;
            if ("ldc-cost-probe-literal-a" != sink) acc++;
            if ("ldc-cost-probe-literal-b" != sink) acc++;
            if ("ldc-cost-probe-literal-c" != sink) acc++;
            if ("ldc-cost-probe-literal-d" != sink) acc++;
            if ("ldc-cost-probe-literal-e" != sink) acc++;
            if ("ldc-cost-probe-literal-f" != sink) acc++;
        }
        return acc;
    }

    static int ldcStringBase(int iters, Object sink) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            if ("ldc-cost-probe-literal-0" != sink) acc++;
        }
        return acc;
    }

    // ldc of a CONSTANT_Class: the arm that ran a full loader-aware
    // resolution BY NAME plus a mirror lookup on every execution.
    static int ldcClassKernel(int iters, Object sink) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            if (String.class != sink) acc++;
            if (Integer.class != sink) acc++;
            if (Long.class != sink) acc++;
            if (Double.class != sink) acc++;
            if (Float.class != sink) acc++;
            if (Short.class != sink) acc++;
            if (Byte.class != sink) acc++;
            if (Character.class != sink) acc++;
            if (Boolean.class != sink) acc++;
            if (Object.class != sink) acc++;
            if (Number.class != sink) acc++;
            if (Math.class != sink) acc++;
            if (System.class != sink) acc++;
            if (Thread.class != sink) acc++;
            if (Runnable.class != sink) acc++;
            if (Comparable.class != sink) acc++;
        }
        return acc;
    }

    static int ldcClassBase(int iters, Object sink) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            if (String.class != sink) acc++;
        }
        return acc;
    }

    // Control: iadd. Must not move between arms.
    static int iaddKernel(int iters, int seed) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
            acc += seed; acc += seed; acc += seed; acc += seed;
        }
        return acc;
    }

    static int iaddBase(int iters, int seed) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += seed;
        }
        return acc;
    }

    static long timeNs(Runnable r) {
        long t0 = System.nanoTime();
        r.run();
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        Object sink = new Object();
        int[] guard = new int[1];

        // Warm-up: every kernel, so tier-up state is the same in both arms.
        for (int w = 0; w < 3; w++) {
            guard[0] += ldcStringKernel(iters / 10, sink);
            guard[0] += ldcStringBase(iters / 10, sink);
            guard[0] += ldcClassKernel(iters / 10, sink);
            guard[0] += ldcClassBase(iters / 10, sink);
            guard[0] += iaddKernel(iters / 10, 3);
            guard[0] += iaddBase(iters / 10, 3);
        }

        long sK = timeNs(() -> guard[0] += ldcStringKernel(iters, sink));
        long sB = timeNs(() -> guard[0] += ldcStringBase(iters, sink));
        long cK = timeNs(() -> guard[0] += ldcClassKernel(iters, sink));
        long cB = timeNs(() -> guard[0] += ldcClassBase(iters, sink));
        long aK = timeNs(() -> guard[0] += iaddKernel(iters, 3));
        long aB = timeNs(() -> guard[0] += iaddBase(iters, 3));

        long per = (long) iters * (UNROLL - 1);
        System.out.printf("ldc-string  %8.1f ns/op%n", (double) (sK - sB) / per);
        System.out.printf("ldc-class   %8.1f ns/op%n", (double) (cK - cB) / per);
        System.out.printf("iadd(ctrl)  %8.1f ns/op%n", (double) (aK - aB) / per);
        System.out.println("guard " + guard[0]);
    }
}
