// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 16, lane review-fuzz5: a deopt storm. Six subclasses of an
// abstract class arrive one at a time, each never loaded before, while a hot
// OSR loop that was compiled under "one implementation only" is running on a
// second thread, and while the main thread's loops are re-warmed after each
// arrival.
//
// Risk targeted: class-hierarchy dependencies and their withdrawal at define
// (`JitCache::invalidate_for_new_subtype`, `CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED`,
// and the wave-15 constant-pool dependency rule `inlined_callee_dependency_class`,
// which names the class the site names, not the declaring class: `Op.apply` is
// declared in `R21Fz5DsBase` and called through `R21Fz5DsOp`), a running
// OSR activation whose dependency dies under it (it must deopt at its next poll
// and resume with the right locals), and long/int loop bounds that wrap. The
// arrivals change the result only through `apply`, so every hash is a pure
// function of the operations and of which ops each loop was handed.
//
// `R21Fz5DsOp` inherits `apply` from `R21Fz5DsBase`, which calls the abstract
// `step`. `Op1` overrides `step`; `Op2` overrides `apply`; `Op3` throws from
// `step` for some inputs (caught in the loop); `Op4` and `Op5` add a level;
// `Op6` overrides both. The worker thread only ever sees `Op0` objects, so its
// hash is the same however the arrivals interleave with it.
//
// Passes: `cold`, `warm` (the loops 60 times over Op0), then `load1..load6`
// (one arrival each, then the loops over every op loaded so far), `worker`
// (the second thread's result, joined), `final`.
//
// Run:
//   javac -d out R21Fz5DeoptStorm1.java
//   java -Xmx256m -cp out R21Fz5DeoptStorm1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz5DeoptStorm1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz5DeoptStorm1
//   cratonvm --nojit -Xmx256m -cp out R21Fz5DeoptStorm1
//   CRATONVM_JIT_SPLICE_DECLARING_CLASS_FRAME=0 cratonvm ...      # control arm
//   CRATONVM_JIT_SUBTYPE_DEFINE_SCOPED=0 cratonvm ...             # control arm
// HotSpot expectation: every line identical on every arm.

import java.util.ArrayList;
import java.util.List;

public class R21Fz5DeoptStorm1 {
    static long mix(long h, long v) {
        return (h ^ v) * 0x100000001B3L;
    }

    // Long bound that wraps: from MAX - n through MAX, then MIN .. MIN + 2,
    // stopping at MIN + 3: n + 4 trips.
    static long longLoop(R21Fz5DsOp op, int n) {
        long h = 1;
        long trips = 0;
        for (long j = Long.MAX_VALUE - n; j != Long.MIN_VALUE + 3; j++) {
            try {
                h = mix(h, op.apply(j));
            } catch (IllegalArgumentException e) {
                h = mix(h, e.getMessage().length());
            }
            trips++;
        }
        return mix(h, trips);
    }

    // Int bound that wraps downward: from MIN + 10 down through MIN, then MAX
    // down to MAX - 8, stopping at MAX - 9: 20 trips per call; called hot.
    static long intLoop(R21Fz5DsOp op) {
        long h = 3;
        int trips = 0;
        for (int i = Integer.MIN_VALUE + 10; i != Integer.MAX_VALUE - 9; i--) {
            try {
                h = mix(h, op.apply(i));
            } catch (IllegalArgumentException e) {
                h = mix(h, -1);
            }
            trips++;
        }
        return mix(h, trips);
    }

    static long round(List<R21Fz5DsOp> ops, int reps) {
        long h = 0;
        for (int r = 0; r < reps; r++) {
            for (R21Fz5DsOp op : ops) {
                h = mix(h, intLoop(op));
            }
        }
        for (R21Fz5DsOp op : ops) {
            h = mix(h, longLoop(op, 50_000));
        }
        return h;
    }

    static volatile boolean stop;
    static long workerResult;
    static int workerRounds;

    public static void main(String[] args) throws Exception {
        List<R21Fz5DsOp> ops = new ArrayList<>();
        ops.add(new R21Fz5DsOp0());
        System.out.println("cold " + round(ops, 1));
        long w = 0;
        for (int r = 0; r < 60; r++) {
            w = mix(w, round(ops, 50));
        }
        System.out.println("warm " + w);

        // The worker: a long OSR loop over Op0 only. It runs a fixed number of
        // rounds, so its result does not depend on timing; `stop` only ends it
        // early if the main thread finishes first (it never does on HotSpot).
        R21Fz5DsOp only = new R21Fz5DsOp0();
        Thread worker = new Thread(() -> {
            long h = 0;
            int rounds = 0;
            for (; rounds < 40 && !stop; rounds++) {
                h = mix(h, longLoop(only, 400_000));
            }
            workerResult = h;
            workerRounds = rounds;
        }, "storm-worker");
        worker.start();

        for (int k = 1; k <= 6; k++) {
            R21Fz5DsOp op = (R21Fz5DsOp) Class.forName("R21Fz5DsOp" + k).getDeclaredConstructor().newInstance();
            ops.add(op);
            long h = 0;
            for (int r = 0; r < 5; r++) {
                h = mix(h, round(ops, 20));
            }
            System.out.println("load" + k + " " + op.getClass().getName() + " " + h);
        }
        worker.join();
        stop = true;
        System.out.println("worker rounds=" + workerRounds + " h=" + workerResult);
        System.out.println("final " + round(ops, 10));
    }
}

abstract class R21Fz5DsBase {
    long apply(long x) {
        return step(x) * 3 + (x >>> 61);
    }

    abstract long step(long x);
}

abstract class R21Fz5DsOp extends R21Fz5DsBase {
}

class R21Fz5DsOp0 extends R21Fz5DsOp {
    long step(long x) {
        return x ^ (x << 7);
    }
}

class R21Fz5DsOp1 extends R21Fz5DsOp {
    long step(long x) {
        return x + 0x7fffffffffffL;
    }
}

class R21Fz5DsOp2 extends R21Fz5DsOp {
    long step(long x) {
        return -x;
    }

    @Override
    long apply(long x) {
        return Long.rotateLeft(x, 13) - step(x);
    }
}

class R21Fz5DsOp3 extends R21Fz5DsOp {
    long step(long x) {
        if ((x & 31) == 7) {
            throw new IllegalArgumentException("op3 " + (x & 1023));
        }
        return x * 5;
    }
}

class R21Fz5DsOp4 extends R21Fz5DsOp1 {
    @Override
    long step(long x) {
        return super.step(x) >> 3;
    }
}

class R21Fz5DsOp5 extends R21Fz5DsOp4 {
    @Override
    long apply(long x) {
        return super.apply(x) ^ Long.MIN_VALUE;
    }
}

class R21Fz5DsOp6 extends R21Fz5DsOp {
    long step(long x) {
        return Math.multiplyHigh(x, 0x9E3779B97F4A7C15L);
    }

    @Override
    long apply(long x) {
        return step(x) + (int) x;
    }
}
