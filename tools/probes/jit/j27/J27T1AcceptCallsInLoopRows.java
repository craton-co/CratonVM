// j27 wave 7, lane T1ACCEPT: ACCEPT21-5's call half (diagnostic only, no gate change).
//
// Book: docs/feature-designs/wire-tiered-manager.md (ACCEPT21-5).
// What it checks: the `[ir] acceptance ...` lines' `[guard-frame ...]` tail now also names the
// Java calls still scheduled inside a loop, ` calls-in-loop=<k>` (printed only when k > 0;
// `ir_evidence::GuardFrameCensus::in_loop_calls`). `inlinedKernel`'s tiny `inc` should be inlined,
// so its line should carry NO `calls-in-loop=`; `outOfLineKernel` calls the self-recursive `rec`,
// whose innermost level stays a call, so its line should say `calls-in-loop=1` or more.
//
// Run (both modes):
//   CRATONVM_DBG_IR_COMPILES=1 <bin> [--compatible] -cp <dir> J27T1AcceptCallsInLoopRows 2>&1 \
//     | grep -E 'acceptance J27T1AcceptCallsInLoopRows\.(inlined|outOfLine)Kernel'
// A `calls-in-loop=` on the inlined kernel means the call was not inlined (read the inline log
// before filing anything). Then the fact lines must equal HotSpot 25 with and without the switch.
//
// HotSpot 25 expected output (exactly these lines, without -Dj27.time=true):
//   fact inlined 500500
//   fact outofline 1001000
// (inlined: sum over i in 0..999 of i + 1 = 1000 * 1001 / 2; outofline: twice that, `rec` returns
// v + 1 at every depth.)
public class J27T1AcceptCallsInLoopRows {
    static int inc(int x) {
        return x + 1;
    }

    static int rec(int d, int v) {
        if (d == 0) {
            return v + 1;
        }
        return rec(d - 1, v);
    }

    static int inlinedKernel(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += inc(i);
        }
        return s;
    }

    static int outOfLineKernel(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += rec(3, i) * 2;
        }
        return s;
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        int a = 0;
        int b = 0;
        for (int w = 0; w < 20000; w++) {
            a = inlinedKernel(1000);
            b = outOfLineKernel(1000);
        }
        System.out.println("fact inlined " + a);
        System.out.println("fact outofline " + b);
        if (time) {
            long t0 = System.nanoTime();
            for (int w = 0; w < 20000; w++) {
                a += inlinedKernel(1000);
            }
            long t1 = System.nanoTime();
            for (int w = 0; w < 20000; w++) {
                b += outOfLineKernel(1000);
            }
            long t2 = System.nanoTime();
            System.out.println("time inlined-ns-per-call " + (t1 - t0) / 20000 + " sink " + a);
            System.out.println("time outofline-ns-per-call " + (t2 - t1) / 20000 + " sink " + b);
        }
    }
}
