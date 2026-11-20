public class NonCanonicalLoops {
    // Non-canonical loop shapes. The analyzer accepts each method
    // (every opcode is individually fine), but the counted-loop
    // recognizer in jit-cuda must REJECT every one of these: the
    // element-wise GPU lowering model ("one thread per iteration,
    // tid IS the loop variable") is only valid for the canonical
    // `for (int i = 0; i < n; i++)` shape. Anything else would be
    // silently mis-lowered, so the loop must fall back to the CPU.

    // `i <= n` — javac emits `if_icmpgt` for the exit. Runs n+1
    // times; lowering it as `tid < n` skips the last element.
    public static void leLoop(int[] a, int n) {
        for (int i = 0; i <= n; i++) {
            a[i] = 0;
        }
    }

    // `i != n` — javac emits `if_icmpne` for the exit. The
    // tid < n dispatch is not equivalent in general.
    public static void neLoop(int[] a, int n) {
        for (int i = 0; i != n; i++) {
            a[i] = 0;
        }
    }

    // Non-unit stride: `i += 2`. iinc constant is +2. Lowering
    // reads element `tid` instead of element `2*tid`.
    public static void stride2Loop(int[] a) {
        int n = a.length;
        for (int i = 0; i < n; i += 2) {
            a[i] = 0;
        }
    }

    // Non-zero start: `i = 5`. Every access is offset by 5.
    public static void start5Loop(int[] a) {
        int n = a.length;
        for (int i = 5; i < n; i++) {
            a[i] = 0;
        }
    }

    // The canonical baseline — must STILL be accepted and lowered.
    public static void canonical(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
