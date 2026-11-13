// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 4 (lane ARGFIX2): owned arguments at a direct call INSIDE a spliced
// body (the two-level replay rule,
// CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1, a sub-switch of
// CRATONVM_JIT_IR_CALLEE_OWNED_ARGS).
// Page: docs/feature-designs/native-dispatch-architecture.md
//
// Shape: `outer` is padded over the 325-byte splice limit (so it is its own
// method-entry optimizing body, entered 42 000 times, no loop, so no OSR);
// `mid` is tiny and is spliced into `outer`; `leaf` is padded too, so the call
// `mid -> leaf` is a real direct call from inside a splice. Both prefixes
// before the invokes are pure (aload, iload), `leaf` has no exception table, is
// not synchronized and makes no invokedynamic (its message uses StringBuilder).
//
// Rows:
//   warm         the steady state, 42 000 calls;
//   throw        `leaf` commits two side effects, then throws: each effect
//                exactly once, and the trace names leaf, mid, outer;
//   deopt        a branch `leaf` never took while warming (a trap in the
//                callee), effects exactly once;
//   after-deopt  the steady state again after the trap;
//   gc           300 calls whose callee churns 4 MiB and sometimes runs
//                System.gc() before it reads its argument again (the callee's
//                own frame must root the argument the caller handed off);
//   big          (only with -Dj27.big=true; a retention row) a 52 MiB argument
//                the callee drops before allocating another 52 MiB at -Xmx96m.
//
//   java -Xmx96m -cp <dir> J27ArgfixSpliceRows > hs.txt
//   cratonvm -Xmx96m --cp <dir> J27ArgfixSpliceRows | diff - hs.txt
//     x {default, CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1,
//        CRATONVM_JIT_IR_CALLEE_OWNED_ARGS=0} x {--real-jdk, --compatible}
//     x {ZGC default, -XX:+UseG1GC, -XX:+UseGenerationalGC}
//   engagement: + CRATONVM_DBG_IR_SLOTS=1 2>&1 | grep 'ir-owned-args\] site.*leaf'
//     expect `... J27ArgfixSpliceRows.leaf([II)I kind=3 admitted=true owned=true splice-replay=Ok(184)`
//
// Expected HotSpot 25 output (facts):
//   fact warm w=357000 effects=42000 a0=42000
//   fact throw java.lang.IllegalStateException leaf k=-1 effects=1 a0=1 after effects=1 a0=1 frames=leaf,mid,outer
//   fact deopt r=4002 effects=101 a0=1
//   fact after-deopt w=8500 effects=1000 a0=1000
//   fact gc r=65534 bad=0 effects=300 a0=300
//   fact pad 0
// With -Dj27.big=true, also (between `gc` and `pad`):
//   fact big ok len=13631488 b=13631488
// CratonVM: that line with CRATONVM_JIT_IR_CALLEE_OWNED_ARGS_SPLICE_REPLAY=1 (window
// engagement: `window n<id> bci=<combined pc> ...: homes [<offset>]` for leafBig);
// `fact big OOME Java heap space` without it (the spliced caller roots the argument).
public class J27ArgfixSpliceRows {
    static int effects;
    static long lastSum;
    static long pad;
    static Object keep;

    static int outer(int[] a, int k) {
        int r = mid(a, k);
        if (r < -1000000) {
            // Never taken: keeps outer over the splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int mid(int[] a, int k) {
        return leaf(a, k) + 1;
    }

    static int leaf(int[] a, int k) {
        effects++;
        a[0] += 1;
        if (k == -1) {
            throw new IllegalStateException(new StringBuilder()
                    .append("leaf k=-1 effects=").append(effects)
                    .append(" a0=").append(a[0]).toString());
        }
        if (k == -2) {
            // Never taken while warming: a trap inside the callee.
            effects += 100;
            return a.length * 1000 + a[0];
        }
        if (k == -3) {
            Object junk = null;
            for (int j = 0; j < 256; j++) {
                junk = new byte[16384];
            }
            keep = junk;
            if (effects % 50 == 0) {
                System.gc();
            }
            long s = 0;
            for (int j = 0; j < a.length; j++) {
                s += a[j];
            }
            lastSum = s;
            keep = null;
            return a.length + k;
        }
        if (k < -1000) {
            // Never taken: keeps leaf over the splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return a.length + k;
    }

    // The retention row's own chain: outerBig allocates the argument (a pure
    // prefix: iload, ishl, newarray, iload), midBig is spliced into it.
    static int outerBig(int mb) {
        int r = midBig(new int[mb << 18], mb);
        if (r < -1000000) {
            // Never taken: keeps outerBig over the splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return r;
    }

    static int midBig(int[] a, int mb) {
        return leafBig(a, mb);
    }

    static int leafBig(int[] a, int mb) {
        int len = a.length;
        // `a` is dead from here.
        int[] b = new int[mb << 18];
        keep = b;
        if (len < 0) {
            // Never taken: keeps leafBig over the splice limit.
            pad = pad * 31L + 1L;
            pad = pad * 33L + 2L;
            pad = pad * 35L + 3L;
            pad = pad * 37L + 4L;
            pad = pad * 39L + 5L;
            pad = pad * 41L + 6L;
            pad = pad * 43L + 7L;
            pad = pad * 45L + 8L;
            pad = pad * 47L + 9L;
            pad = pad * 49L + 10L;
            pad = pad * 51L + 11L;
            pad = pad * 53L + 12L;
            pad = pad * 55L + 13L;
            pad = pad * 57L + 14L;
            pad = pad * 59L + 15L;
            pad = pad * 61L + 16L;
            pad = pad * 63L + 17L;
            pad = pad * 65L + 18L;
            pad = pad * 67L + 19L;
            pad = pad * 69L + 20L;
            pad = pad * 71L + 21L;
            pad = pad * 73L + 22L;
            pad = pad * 75L + 23L;
            pad = pad * 77L + 24L;
            pad = pad * 79L + 25L;
            pad = pad * 81L + 26L;
            pad = pad * 83L + 27L;
            pad = pad * 85L + 28L;
            pad = pad * 87L + 29L;
            pad = pad * 89L + 30L;
            pad = pad * 91L + 31L;
            pad = pad * 93L + 32L;
        }
        return len;
    }

    static int warm(int[] a, int calls) {
        int w = 0;
        for (int i = 0; i < calls; i++) {
            w += outer(a, i & 7);
        }
        return w;
    }

    public static void main(String[] args) throws InterruptedException {
        int[] a = new int[4];
        int w = warm(a, 41000);
        Thread.sleep(300);
        w += warm(a, 1000);
        System.out.println("fact warm w=" + w + " effects=" + effects + " a0=" + a[0]);

        effects = 0;
        a = new int[4];
        try {
            outer(a, -1);
            System.out.println("fact throw none");
        } catch (IllegalStateException e) {
            StackTraceElement[] st = e.getStackTrace();
            String frames = st.length >= 3
                    ? st[0].getMethodName() + "," + st[1].getMethodName() + "," + st[2].getMethodName()
                    : "short" + st.length;
            System.out.println("fact throw " + e.getClass().getName() + " " + e.getMessage()
                    + " after effects=" + effects + " a0=" + a[0] + " frames=" + frames);
        }

        effects = 0;
        a = new int[4];
        int r = outer(a, -2);
        System.out.println("fact deopt r=" + r + " effects=" + effects + " a0=" + a[0]);

        effects = 0;
        a = new int[4];
        w = warm(a, 1000);
        System.out.println("fact after-deopt w=" + w + " effects=" + effects + " a0=" + a[0]);

        effects = 0;
        int[] g = new int[1 << 16];
        for (int i = 0; i < g.length; i++) {
            g[i] = i;
        }
        int bad = 0;
        r = 0;
        for (int n = 1; n <= 300; n++) {
            r = outer(g, -3);
            if (lastSum != 2147450880L + n) {
                bad++;
            }
        }
        System.out.println("fact gc r=" + r + " bad=" + bad + " effects=" + effects + " a0=" + g[0]);

        if (Boolean.getBoolean("j27.big")) {
            long z = 0;
            for (int i = 0; i < 41000; i++) {
                z += outerBig(0);
            }
            Thread.sleep(300);
            for (int i = 0; i < 1000; i++) {
                z += outerBig(0);
            }
            keep = null;
            try {
                int len = outerBig(52);
                int[] b = (int[]) keep;
                keep = null;
                System.out.println("fact big ok len=" + (len + z) + " b=" + b.length);
            } catch (OutOfMemoryError e) {
                keep = null;
                System.out.println("fact big OOME " + e.getMessage());
            }
        }
        System.out.println("fact pad " + pad);
    }
}
