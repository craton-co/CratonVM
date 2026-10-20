// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 19, lane L4: the non-virtual invoke door now
// counts and tiers up the `Bytecode` entries it serves (private methods,
// super calls, constructors), and declines a callee with a compiled body to
// the general arm that enters it (`invoke_fast::nonvirtual_door_finish`,
// kill switch `SPECIAL_DOOR_TIERS_UP`). Before, a callee served by the door
// was never counted and always ran interpreted.
//
//   private-same     - a private instance method called from its own class
//   private-nestmate - a private method of a nested class called from the
//                      outer class (`invokevirtual` of a private nestmate
//                      method, the virtual door's hand-off)
//   super-call       - an override that calls `super.step(i)`
//   ctor             - `new Point(a, b)` (its `<init>` and `Object.<init>`)
//   private-sync     - a private `synchronized` method
//
// What to measure: each row's ns/call on stderr, JIT on, interleaved against
// the previous build (medians of several runs; in-JVM timings swing ~3x).
// `CRATONVM_DBG=jit-method-stats` should now list `mix`, `Inner.priv`,
// `Base.step`, `Point.<init>` and `locked` as compiled from the invocation
// counter (`Object.<init>`, whose body returns at pc 0, is deliberately left
// interpreted by the door). A row that gets SLOWER is the regression to report: a compiled
// private callee paid two `jit_cache` probes per call through the door in
// wave 19 (interpreter-L4-special-arm-compiled-callee-probes-jit-cache-twice-per-call).
// Wave 20 (lane L3) stores the site as a `Jit` entry once the general arm has
// found the body (`invoke_fast::NONVIRTUAL_ARM_UPGRADES_TO_JIT`): compare the
// `private-same`, `private-nestmate`, `super-call` and `ctor` rows against
// waves 18 and 19. With `CRATONVM_DBG_FIELD_SITE=1` the decline reason
// "special: callee has a compiled body" should read about once per site, and
// "special: cached target is a compiled body" carries the rest.
//
// Stdout is a deterministic checksum per row and must match HotSpot 25, with
// and without `--nojit`.
public class SpecialDoorTierUpBench {
    static final int WARMUP = 20_000;
    static final int ITERS = 400_000;

    private int salt = 7;

    private int mix(int x) {
        int h = x * 0x9E3779B1 + salt;
        return h ^ (h >>> 15);
    }

    private synchronized int locked(int x) {
        salt = (salt * 31 + x) & 0xFFFF;
        return salt;
    }

    static final class Inner {
        private final int bias;

        Inner(int bias) {
            this.bias = bias;
        }

        private int priv(int x) {
            return (x ^ bias) + (x >>> 3);
        }
    }

    static class Base {
        int step(int x) {
            return x * 3 + 1;
        }
    }

    static class Derived extends Base {
        @Override
        int step(int x) {
            return super.step(x) ^ 0x55;
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

    interface Row {
        long run(int iters);
    }

    static void row(String name, Row r) {
        r.run(WARMUP);
        long t0 = System.nanoTime();
        long sum = r.run(ITERS);
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + sum);
        System.err.printf("%-17s %8.1f ns/call%n", name, (double) ns / ITERS);
    }

    long privateSame(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += mix(i);
        }
        return sum;
    }

    long privateSync(int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += locked(i);
        }
        return sum;
    }

    static long privateNestmate(Inner in, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += in.priv(i);
        }
        return sum;
    }

    static long superCall(Base b, int iters) {
        long sum = 0;
        for (int i = 0; i < iters; i++) {
            sum += b.step(i);
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

    public static void main(String[] args) {
        SpecialDoorTierUpBench self = new SpecialDoorTierUpBench();
        Inner inner = new Inner(0x2A);
        Base derived = new Derived();
        row("private-same", self::privateSame);
        row("private-nestmate", n -> privateNestmate(inner, n));
        row("super-call", n -> superCall(derived, n));
        row("ctor", SpecialDoorTierUpBench::ctor);
        row("private-sync", self::privateSync);
    }
}
