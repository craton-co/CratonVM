// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Minimal repro for a branch that changes its answer once the enclosing
// loop is JIT-compiled.
//
// `which` never changes, so exactly one arm of the chain can ever be
// taken. Under the interpreter that holds. With the JIT on, the arm
// taken flips partway through the loop — at the iteration count where
// OSR promotes the loop.
//
// Found 2026-09-02 while differentially testing the GPU emitter: the
// harness's own `switch`-like dispatch changed arms at index 256, which
// looked like a GPU miscompile until `--nojit` and a no-`--gpu` run
// placed it on the CPU side.
//
// Usage: java JitStringBranch [which] [n]
public class JitStringBranch {
    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "f2i";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 4096;

        String last = null;
        int flips = 0;
        int firstFlip = -1;
        int cD = 0, cF = 0, cL = 0, cO = 0;
        for (int i = 0; i < n; i++) {
            String taken;
            if (which.equals("d2i")) {
                taken = "d2i";
            } else if (which.equals("f2i")) {
                taken = "f2i";
            } else if (which.equals("f2l")) {
                taken = "f2l";
            } else {
                taken = "other";
            }
            if (taken == "d2i") cD++;
            else if (taken == "f2i") cF++;
            else if (taken == "f2l") cL++;
            else cO++;
            if (last != null && !last.equals(taken)) {
                if (firstFlip < 0) {
                    firstFlip = i;
                }
                flips++;
            }
            last = taken;
        }
        System.out.println("which=" + which + " n=" + n
                + " flips=" + flips + " firstFlip=" + firstFlip
                + " final=" + last
                + " counts[d2i=" + cD + " f2i=" + cF + " f2l=" + cL + " other=" + cO + "]");
    }
}
