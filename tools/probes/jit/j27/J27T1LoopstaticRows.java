// j27 wave 6, lane T1LOOPSTATIC: the rows of LOOPSTATIC21-1..4
// (docs/feature-designs/jit-ir-optimizations.md). The kernel is
// R20PLoopStaticBarrier's (`a.v += k * SCALE + i; a.w += bias;` over a fresh accumulator), plus
// a loop that reads one static and writes another (the census row of LOOPSTATIC21-1).
//
// Expected output on HotSpot 25 (both CratonVM modes must print the same `fact` lines):
//   fact one-call-osr 1250000725000000,100000000
//   fact static-read-loop 514500,2000
//   fact acc-fresh-nostatic 514500,2000
//   fact acc-fresh-static-method 514500,2000
//   fact run-zero-trip 0,0
//   fact run-after-static-change 554500,2000
//   fact run-after-static-restore 514500,2000
//   fact static-read-putstatic-loop 1498500
//   fact static-read-putstatic-count 1000
//   fact static-read-putstatic-after-change 5491500
// Derivation: sum_{i<n} (c + i) = n*c + n(n-1)/2. n = 1000: 499500 + 1000*c; c = 5*3 = 15 ->
// 514500; c = 5*11 = 55 -> 554500; w = 2*n. one-call: n = 50_000_000, c = 15:
// 750_000_000 + 1_249_999_975_000_000 = 1_250_000_725_000_000, w = 100_000_000.
// putstatic row: SCALE * sum_{i<1000} i = 3 * 499500 = 1498500; after SCALE = 11 the row prints
// 11 * 499500 - 3000 = 5491500 (a stale SCALE = 3 would print 1495500); COUNT grows by n = 1000
// per call.
//
// Timing lines (`time <row> <ns/call>`) only with -Dj27.time=true.
//
// Diagnosis run (one, --real-jdk; keep stderr):
//   CRATONVM_DBG_JITC=1 CRATONVM_DBG_LOOP_FIELD_FORWARD=1 CRATONVM_DBG_UNROLL=1 CRATONVM_DBG_LICM=1 \
//     cratonvm -Xmx512m -cp <out> J27T1LoopstaticRows 2> dbg.txt
//   * LOOPSTATIC21-1: grep 'static read census' dbg.txt -- one line per loop that kept a direct
//     getstatic, naming the node that kept it (putReadLoop: the StoreStatic of COUNT in the early
//     run; a promoted loop must NOT appear after its `loop static promote` line). Compare with
//     grep -c 'loop side effect LoadStatic' dbg.txt.
//   * LOOPSTATIC21-2: grep 'J27T1LoopstaticRows$Once.run' dbg.txt -- `base n<b> is an OSR local`
//     is the fresh promotion's answer on the door; the `loop field promote: header bci <b>` line
//     just above it (same compile) is promote_loop_cells' answer for the same accumulator.
//   * LOOPSTATIC21-3: grep 'promoted header bci' dbg.txt -- `offered` / `not offered -- <why>` for
//     accNoStatic's compile; when offered, the next `[DBG_UNROLL] region ...` line is the answer.
//   * LOOPSTATIC21-4: grep -E 'runStatic' dbg.txt -- which body (c2-supersede / OSR) serves it and
//     whether `loop fresh field promote: ...runStatic` appears.
public class J27T1LoopstaticRows {
    static int SCALE = 3;
    static int COUNT;
    static long sink;

    static final class Acc {
        long v;
        long w;
    }

    final int k;
    final int bias;

    J27T1LoopstaticRows(int k, int bias) {
        this.k = k;
        this.bias = bias;
    }

    Acc run(int n) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += k * SCALE + i;
            a.w += bias;
        }
        return a;
    }

    Acc accNoStatic(int n, int scale) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += k * scale + i;
            a.w += bias;
        }
        return a;
    }

    static Acc runStatic(int n, int kk, int b) {
        Acc a = new Acc();
        for (int i = 0; i < n; i++) {
            a.v += kk * 3 + i;
            a.w += b;
        }
        return a;
    }

    // LOOPSTATIC21-1 census row: reads SCALE, writes COUNT (a direct putstatic).
    static long putReadLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (long) SCALE * i;
            COUNT++;
        }
        return s;
    }

    static final class Once {
        final int k;
        final int bias;

        Once(int k, int bias) {
            this.k = k;
            this.bias = bias;
        }

        Acc run(int n) {
            Acc a = new Acc();
            for (int i = 0; i < n; i++) {
                a.v += k * SCALE + i;
                a.w += bias;
            }
            return a;
        }
    }

    static void fact(String row, Acc a) {
        System.out.println("fact " + row + " " + a.v + "," + a.w);
    }

    interface Body {
        long run();
    }

    static void time(String row, Body b, long calls) {
        long[] ns = new long[7];
        for (int w = 0; w < 3; w++) {
            sink += b.run();
        }
        for (int r = 0; r < ns.length; r++) {
            long t0 = System.nanoTime();
            sink += b.run();
            ns[r] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(ns);
        System.out.printf("time %s %.1f%n", row, ns[ns.length / 2] / (double) calls);
    }

    public static void main(String[] args) {
        // LOOPSTATIC21-2 first, before anything warms `Once.run`: one call, OSR door only.
        fact("one-call-osr", new Once(5, 2).run(50_000_000));

        J27T1LoopstaticRows p = new J27T1LoopstaticRows(5, 2);
        for (int i = 0; i < 60_000; i++) {
            sink += p.run(i & 1023).v + p.accNoStatic(i & 1023, 3).v
                    + runStatic(i & 1023, 5, 2).v + putReadLoop(i & 1023);
        }
        fact("static-read-loop", p.run(1000));
        fact("acc-fresh-nostatic", p.accNoStatic(1000, 3));
        fact("acc-fresh-static-method", runStatic(1000, 5, 2));
        fact("run-zero-trip", p.run(0));
        SCALE = 11; // the compiled body must see the new value on its next call
        fact("run-after-static-change", p.run(1000));
        SCALE = 3;
        fact("run-after-static-restore", p.run(1000));

        System.out.println("fact static-read-putstatic-loop " + putReadLoop(1000));
        int before = COUNT;
        putReadLoop(1000);
        System.out.println("fact static-read-putstatic-count " + (COUNT - before));
        SCALE = 11;
        System.out.println("fact static-read-putstatic-after-change " + (putReadLoop(1000) - 3000));
        SCALE = 3;

        if (!Boolean.getBoolean("j27.time")) {
            return;
        }
        time("static-read-loop", () -> {
            long s = 0;
            for (int i = 0; i < 60_000; i++) {
                s += p.run(1000).v;
            }
            return s;
        }, 60_000);
        time("acc-fresh-nostatic", () -> {
            long s = 0;
            for (int i = 0; i < 60_000; i++) {
                s += p.accNoStatic(1000, 3).v;
            }
            return s;
        }, 60_000);
        time("acc-fresh-static-method", () -> {
            long s = 0;
            for (int i = 0; i < 60_000; i++) {
                s += runStatic(1000, 5, 2).v;
            }
            return s;
        }, 60_000);
        time("static-read-putstatic-loop", () -> {
            long s = 0;
            for (int i = 0; i < 60_000; i++) {
                s += putReadLoop(1000);
            }
            return s;
        }, 60_000);
    }
}
