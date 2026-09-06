// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Does a compiled caller still offload when its kernel lives in ANOTHER CLASS?
//
// `offload_jit_gate` populates the compiled-tier offload registry as a side
// effect of scanning CALLERS, and it can only judge a call target whose
// declaring class is already loaded. The module docs called that the "forward
// references" limitation and argued it as a corner case. It is not a corner
// case: a caller is scanned when it is admitted to the JIT, which happens
// BEFORE it runs, so a callee in another class has typically never been
// touched at that moment. A kernel in the caller's own class is loaded by
// construction; one in a second class is not.
//
// The two arms below differ in NOTHING but which class the kernel is declared
// in. Same body, same signature, same driver method, same iteration count:
//
//   sameclass  : GpuForwardRef.scaleHere   -- the caller's own class, so the
//                gate can always judge it. This is the CONTROL, and it is what
//                every other GPU fixture in the tree happens to look like:
//                GpuLdcSplit, GpuIntensitySweep, GpuProbe and GpuWarm all
//                declare their kernels beside their drivers, which is why none
//                of them could ever have found this.
//   otherclass : GpuForwardRefKernel.scale -- a second class, loaded by the
//                very call the gate is trying to predict.
//
// Both must offload. Before 2026-09-06 `otherclass` did not, and the answer
// was still right -- it just ran on the CPU, which is why the checksum below
// cannot gate this and the dispatch witness has to.
//
// Usage: java GpuForwardRef <sameclass|otherclass> <n> <iters>
public class GpuForwardRef {

    // The kernel, declared in the DRIVER's own class. Loaded by construction
    // the moment anything calls `drive`, so `offload_jit_gate` can always
    // judge it.
    static void scaleHere(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7;   v = v * 5 + 11;  v = v * 7 - 13;  v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            v = v * 3 - 7;   v = v * 5 + 11;  v = v * 7 - 13;  v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            out[i] = v;
        }
    }

    // The caller under test. Both call sites are in its bytecode from the
    // first byte; only one of them is taken per run, and only one of them
    // names a class the gate can resolve when it scans this method.
    static int drive(int mode, int[] in, int[] out) {
        if (mode == 0) {
            scaleHere(in, out);
        } else {
            GpuForwardRefKernel.scale(in, out);
        }
        return out[0];
    }

    public static void main(String[] args) {
        String arm = args[0];
        int n = Integer.parseInt(args[1]);
        int iters = Integer.parseInt(args[2]);
        int mode = arm.equals("sameclass") ? 0 : 1;

        int[] in = new int[n], out = new int[n];
        for (int i = 0; i < n; i++) in[i] = i;

        int sink = 0;
        long t0 = System.nanoTime();
        for (int k = 0; k < iters; k++) sink += drive(mode, in, out);
        long t1 = System.nanoTime();

        long s = 0;
        for (int i = 0; i < n; i += 4096) s += out[i];
        System.out.println("arm=" + arm + " checksum=" + s + " sink=" + (sink & 1)
                + " kernel_ns=" + (t1 - t0));
    }
}

// The identical kernel, declared somewhere the gate cannot see when it scans
// `drive`. This class is loaded by the very `invokestatic` whose fate is being
// decided.
class GpuForwardRefKernel {
    static void scale(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) {
            int v = in[i];
            v = v * 3 - 7;   v = v * 5 + 11;  v = v * 7 - 13;  v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            v = v * 3 - 7;   v = v * 5 + 11;  v = v * 7 - 13;  v = v * 11 + 17;
            v = v * 13 - 19; v = v * 17 + 23; v = v * 19 - 29; v = v * 23 + 31;
            out[i] = v;
        }
    }
}
