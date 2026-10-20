// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 104, lane L2 -- page
// i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range.
//
// The `CRATONVM_DBG_JITC=1` battery on w104 printed `pc-unknown typed match`
// lines for `L2W102ForeignThrowStamp` only, whose shapes are called through
// METHOD REFERENCES (lambda proxies). The lambda arm of the MIC door's callee
// service offered an unstamped throwable to the impl's table with the pc
// unknown: when the impl's body had been retired (C1 -> C2 supersede) the
// not-entrant stub had already run the impl through the VM and searched its
// table at the real pc, but the escape record it left names the IMPL while the
// service asked about the SAM method (`call`), so it never matched, and a typed
// row matched on class alone. The impl's handler then ran for a throw it does
// not cover, and the code after it ran again. `L2W102ForeignThrowStamp` cannot
// see that (its handler and its re-run are idempotent); this probe counts.
//
// Every caller here bumps `HANDLER` in its handler and `CALLS` per entry, and
// `thrower` bumps `THROWS`. HotSpot 25, every row:
//   <name> handler=0 calls=<ITER> throws=<ITER/1000> escaped=<ITER/1000>
// except `inside`, whose handler covers the call:
//   inside handler=<ITER/1000> calls=<ITER> throws=<ITER/1000> escaped=0
// Several fresh classes per shape (rounds) give the background compiler more
// supersedes to race with. Run by default, with `CRATONVM_BG_COMPILE=0`, with
// `--nojit`, both modes, and a few copies at once for load; with
// `CRATONVM_DBG_JITC=1`, `grep -c JITC-PC-CENSUS` should be 0.
public class L2W104LambdaImplHandlerRerun {
    static final int ITER = 60_000;
    static int HANDLER;
    static int CALLS;
    static int THROWS;

    static int thrower(int k) {
        if (k % 1000 == 999) {
            THROWS++;
            throw new IllegalStateException("t");
        }
        return k & 7;
    }

    static int safe(int k) {
        return k & 3;
    }

    static int outsideCaller(int k) {
        CALLS++;
        int r = 0;
        try {
            r = safe(k);
            r += safe(k + 1);
        } catch (IllegalStateException e) {
            HANDLER++;
            r = -1000;
        }
        r += thrower(k);
        return r;
    }

    static int middle(int k) {
        return thrower(k) + 1;
    }

    static int nestedCaller(int k) {
        CALLS++;
        int r = 0;
        try {
            r = safe(k);
            r += safe(k + 1);
        } catch (IllegalStateException e) {
            HANDLER++;
            r = -1000;
        }
        r += middle(k);
        return r;
    }

    static int insideCaller(int k) {
        CALLS++;
        int r = 0;
        try {
            r = safe(k);
            r += thrower(k);
        } catch (IllegalStateException e) {
            HANDLER++;
            r = -1000;
        }
        return r;
    }

    // A capturing lambda: the proxy reads a field before it calls the impl.
    static int capturedCaller(int base, int k) {
        CALLS++;
        int r = base;
        try {
            r += safe(k);
            r += safe(k + 1);
        } catch (IllegalStateException e) {
            HANDLER++;
            r = -1000;
        }
        r += thrower(k);
        return r;
    }

    interface Shape {
        int call(int k);
    }

    static void run(String name, Shape s) {
        HANDLER = 0;
        CALLS = 0;
        THROWS = 0;
        int escaped = 0;
        for (int k = 0; k < ITER; k++) {
            try {
                s.call(k);
            } catch (RuntimeException e) {
                escaped++;
            }
        }
        System.out.println(name + " handler=" + HANDLER + " calls=" + CALLS + " throws=" + THROWS
                + " escaped=" + escaped);
    }

    public static void main(String[] args) {
        int base = args.length;
        for (int round = 0; round < 3; round++) {
            run("outside", L2W104LambdaImplHandlerRerun::outsideCaller);
            run("nested", L2W104LambdaImplHandlerRerun::nestedCaller);
            run("inside", L2W104LambdaImplHandlerRerun::insideCaller);
            run("captured", k -> capturedCaller(base, k));
            run("anonymous", new Shape() {
                public int call(int k) {
                    return outsideCaller(k);
                }
            });
        }
    }
}
