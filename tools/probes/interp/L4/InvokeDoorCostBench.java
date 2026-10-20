// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 22, lane L4: the per-call cost of the three
// interpreter invoke doors (`vm/src/runtime/interpreter/invoke_fast.rs`,
// `dispatch_virtual.rs::execute_invokevirtual_fast_door`) after four changes:
//
//   a. the special door's tier-up block is out of line (`door_tier_up`) and
//      the door counter is a plain increment (`DOOR_COUNT_IS_PLAIN`) instead
//      of a `lock cmpxchg` per counted call;
//   b. every warm door call MOVES the callee's `Arc` into the reused frame
//      slot (`push_frame_verbatim`; and `install_cached_frame` for the
//      general dispatchers) instead of cloning it and dropping the original
//      (two atomic refcount updates per call);
//   c. the special door hands the entry it declined on its kind to the
//      general dispatcher (`SPECIAL_DOOR_HANDS_OVER_ITS_PROBE`), so a
//      compiled super call or constructor reached from interpreted code
//      probes the inline cache once, not twice;
//   d. the virtual and static doors answer an empty body without a frame
//      (`VIRTUAL_DOOR_ELIDES_EMPTY_BODY`, `STATIC_DOOR_ELIDES_EMPTY_BODY`),
//      as the special door does since wave 20.
//
//   private-same   - a private instance method of the caller's class
//                    (`invokevirtual` private: the virtual door's hand-off)
//   super-call     - `super.step(i)` in the loop itself (`invokespecial`)
//   ctor           - `new Point(i, j)` (`invokespecial Point.<init>`)
//   static-call    - a static method with a body
//   virtual-mono   - a monomorphic `invokevirtual` with a body
//   empty-virtual  - a monomorphic `invokevirtual` of an empty method (d)
//   empty-iface    - an `invokeinterface` of an empty method (d)
//   empty-static   - an `invokestatic` of an empty method (d)
//
// How to run (the loops must stay interpreted, so each row's loop runs in
// ONE invocation; OSR would compile it):
//
//   cratonvm --nojit InvokeDoorCostBench             (rows a/b/d: every row)
//   CRATONVM_JIT_OSR=0 cratonvm InvokeDoorCostBench  (rows c: super-call and
//       ctor, whose callees compile from the invocation counter while the
//       loop stays interpreted; and a: private-same, static-call)
//
// A/B: interleave against the wave-21 build (`fe759dd75` + dev), medians of
// 3+, ns/call on stderr. Expected: `--nojit` every row flat or faster, the
// empty-* rows clearly faster (a frame push and pop fewer, cf.
// `L3/EmptyCtorBench` empty-private 116 -> 73 ns in wave 20); with
// `CRATONVM_JIT_OSR=0`, super-call and ctor faster (one `InvokeCache::get`
// fewer per call), the rest flat or faster. For the wave-19 regression page
// (`--nojit` +6-9% on `L3/SpecialDoorTierUpBench`), run that bench too,
// `--nojit`, A/B against wave 18 and against this build with
// `invoke_fast::SPECIAL_DOOR_TIERS_UP = false` (the only clean A/B of the
// special door's tier-up: wave 18 -> 19 changed much else on the call path).
//
// Stdout is a deterministic checksum per row and must equal HotSpot 25's,
// with and without `--nojit`. HotSpot 25 (25.0.3, default and `-Xint`) prints:
//
//   private-same -325033145
//   super-call 239999800000
//   ctor 40000000000
//   static-call 160000000000
//   virtual-mono 119999800000
//   empty-virtual 400000
//   empty-iface 400000
//   empty-static 400000
public class InvokeDoorCostBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 400_000;

    private int salt = 7;

    private int mix(int x) {
        int h = x * 0x9E3779B1 + salt;
        return h ^ (h >>> 15);
    }

    static class Base {
        int step(int x) {
            return x * 3 + 1;
        }
    }

    static final class Derived extends Base {
        @Override
        int step(int x) {
            return x ^ 0x55;
        }

        long superLoop(int iters) {
            long sum = 0;
            for (int i = 0; i < iters; i++) {
                sum += super.step(i);
            }
            return sum;
        }
    }

    static final class Point {
        final int a;
        final int b;

        Point(int a, int b) {
            this.a = a;
            this.b = b;
        }
    }

    static class Counter {
        int n;

        int bump(int x) {
            n += x & 1;
            return n + x;
        }
    }

    interface Listener {
        void onEvent(int x);
    }

    static final class Quiet implements Listener {
        @Override
        public void onEvent(int x) {
        }

        void hook(int x) {
        }
    }

    static void emptyStatic(int x) {
    }

    static int staticBody(int x) {
        return (x << 1) + 1;
    }

    interface Row {
        long run(int iters);
    }

    static void row(String name, Row r) {
        r.run(WARMUP);
        long t0 = System.nanoTime();
        long sum = r.run(ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf("%-15s %8.1f ns/call%n", name, (double) ns / ITERS);
    }

    long privateSame(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += mix(i);
        }
        return sum;
    }

    static long ctor(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            Point p = new Point(i, i >>> 1);
            sum += p.a - p.b;
        }
        return sum;
    }

    static long staticCall(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += staticBody(i);
        }
        return sum;
    }

    static long virtualMono(Counter c, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += c.bump(i);
        }
        return sum;
    }

    static long emptyVirtual(Quiet q, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            q.hook(i);
            sum++;
        }
        return sum;
    }

    static long emptyIface(Listener l, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            l.onEvent(i);
            sum++;
        }
        return sum;
    }

    static long emptyStaticLoop(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            emptyStatic(i);
            sum++;
        }
        return sum;
    }

    public static void main(String[] args) {
        InvokeDoorCostBench self = new InvokeDoorCostBench();
        Derived derived = new Derived();
        Quiet quiet = new Quiet();
        row("private-same", self::privateSame);
        row("super-call", derived::superLoop);
        row("ctor", InvokeDoorCostBench::ctor);
        row("static-call", InvokeDoorCostBench::staticCall);
        // A fresh counter per row run keeps the checksum independent of the
        // warmup (its field accumulates).
        row("virtual-mono", n -> virtualMono(new Counter(), n));
        row("empty-virtual", n -> emptyVirtual(quiet, n));
        row("empty-iface", n -> emptyIface(quiet, n));
        row("empty-static", InvokeDoorCostBench::emptyStaticLoop);
    }
}
