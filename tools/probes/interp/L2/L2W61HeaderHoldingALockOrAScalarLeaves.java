// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L2: shape 2 (b) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md
// -- an optimizing-tier loop whose header state HOLDS A LOCK (a
// `synchronized` block around the loop) or names a SCALAR-REPLACED object (a
// non-escaping accumulator), measured. A spinning compiled activation must
// leave when the class of a callee it spliced is retransformed: every
// iteration after the retransform reads the NEW `Callee.value()`.
//
// Each row's method runs `n` iterations; at iteration `parkAt` it sets a flag
// and spins (an inner loop, whose header holds the same lock / names the same
// object) until main lets it go. Meanwhile main retransforms Callee. The row
// prints how many of the iterations after `parkAt` read the new value.
//   lock          `synchronized (LOCK) { for ... }`, compiled at method entry
//                 (warmed by 20,000 calls), then called once with the park;
//   scalar        an `Acc` accumulator that never escapes, same protocol;
//   plain         neither (the control: such a header has its exit since
//                 interpreter round i1 wave 15);
//   lock-osr      `lock` in one long call (an OSR body);
//   scalar-osr    `scalar` in one long call;
//   lock-again    the first row again, last (cold-VM first-call gaps).
//
// HOTSPOT_EXPECTED_BEGIN (Temurin 25.0.3 with the agent, default flags and -Xint)
// lock new-after=499
// scalar new-after=499
// plain new-after=499
// lock-osr new-after=499
// scalar-osr new-after=499
// lock-again new-after=499
// HOTSPOT_EXPECTED_END
// Without the agent both VMs print `no agent`.
//
// CratonVM (lane's Windows debug build, base 1da4cc4eb9, measured): HotSpot's
// lines with default flags, `CRATONVM_TIER_C2_THRESHOLD=1000000000`,
// `--compatible` and `--nojit`. A MEASUREMENT probe (no fix): under
// `CRATONVM_DBG_JITC=1` the optimizing tier declines both lock methods
// (`IrBuilder::build returned None`) and keeps the method-entry scalar
// method single-pass ("a deopt point names an elided allocation it cannot
// describe"), and the OSR compile of `scalarOsr` sees `acc` as a frame local;
// so no row reaches 2 (b)'s lock or scalar refusal, and every single-pass
// body leaves. A row printing less than 499 means a body finished its
// activation on the old splice: compare `[ir-mode-exit-refused]` lines.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W61HeaderHoldingALockOrAScalarLeaves$Agent
//     Can-Retransform-Classes: true
// containing L2W61HeaderHoldingALockOrAScalarLeaves*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W61HeaderHoldingALockOrAScalarLeaves
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L2W61HeaderHoldingALockOrAScalarLeaves {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        static int value() {
            return 12345;
        }
    }

    /** Rewrites `sipush 12345; ireturn` to answer 12345 + n on the n-th transform. */
    static final class Bump implements ClassFileTransformer {
        int bumps;

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"L2W61HeaderHoldingALockOrAScalarLeaves$Callee".equals(className)) {
                return null;
            }
            bumps++;
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = (byte) (0x39 + bumps);
                }
            }
            return out;
        }
    }

    static final class Acc {
        long n;
    }

    static final Object LOCK = new Object();
    static volatile boolean parked;
    static volatile boolean go;
    static volatile long result;

    static long lock(int n, int parkAt) {
        int before = Callee.value();
        long newAfter = 0;
        synchronized (LOCK) {
            for (int i = 0; i < n; i++) {
                if (i == parkAt) {
                    parked = true;
                    while (!go) {
                        // spin, holding LOCK
                    }
                }
                if (i > parkAt && Callee.value() != before) {
                    newAfter++;
                }
            }
        }
        return newAfter;
    }

    static long scalar(int n, int parkAt) {
        int before = Callee.value();
        Acc acc = new Acc();
        for (int i = 0; i < n; i++) {
            if (i == parkAt) {
                parked = true;
                while (!go) {
                    // spin, `acc` live
                }
            }
            if (i > parkAt && Callee.value() != before) {
                acc.n++;
            }
        }
        return acc.n;
    }

    static long plain(int n, int parkAt) {
        int before = Callee.value();
        long newAfter = 0;
        for (int i = 0; i < n; i++) {
            if (i == parkAt) {
                parked = true;
                while (!go) {
                    // spin
                }
            }
            if (i > parkAt && Callee.value() != before) {
                newAfter++;
            }
        }
        return newAfter;
    }

    static long lockOsr(int n, int parkAt) {
        int before = Callee.value();
        long newAfter = 0;
        synchronized (LOCK) {
            for (int i = 0; i < n; i++) {
                if (i == parkAt) {
                    parked = true;
                    while (!go) {
                        // spin, holding LOCK
                    }
                }
                if (i > parkAt && Callee.value() != before) {
                    newAfter++;
                }
            }
        }
        return newAfter;
    }

    static long scalarOsr(int n, int parkAt) {
        int before = Callee.value();
        Acc acc = new Acc();
        for (int i = 0; i < n; i++) {
            if (i == parkAt) {
                parked = true;
                while (!go) {
                    // spin, `acc` live
                }
            }
            if (i > parkAt && Callee.value() != before) {
                acc.n++;
            }
        }
        return acc.n;
    }

    static long lockAgain(int n, int parkAt) {
        int before = Callee.value();
        long newAfter = 0;
        synchronized (LOCK) {
            for (int i = 0; i < n; i++) {
                if (i == parkAt) {
                    parked = true;
                    while (!go) {
                        // spin, holding LOCK
                    }
                }
                if (i > parkAt && Callee.value() != before) {
                    newAfter++;
                }
            }
        }
        return newAfter;
    }

    interface Row {
        long run(int n, int parkAt);
    }

    static volatile long sink;

    static void warm(Row row) {
        go = true;
        long s = 0;
        for (int k = 0; k < 20_000; k++) {
            s += row.run(1000, 500);
        }
        sink = s;
    }

    static void row(String name, Row row, int n) throws Exception {
        parked = false;
        go = false;
        Thread t = new Thread(() -> result = row.run(n, n - 500));
        t.start();
        while (!parked) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        inst.retransformClasses(Callee.class);
        Thread.sleep(50);
        go = true;
        t.join();
        System.out.println(name + " new-after=" + result);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        warm(L2W61HeaderHoldingALockOrAScalarLeaves::lock);
        row("lock", L2W61HeaderHoldingALockOrAScalarLeaves::lock, 1000);
        warm(L2W61HeaderHoldingALockOrAScalarLeaves::scalar);
        row("scalar", L2W61HeaderHoldingALockOrAScalarLeaves::scalar, 1000);
        warm(L2W61HeaderHoldingALockOrAScalarLeaves::plain);
        row("plain", L2W61HeaderHoldingALockOrAScalarLeaves::plain, 1000);
        row("lock-osr", L2W61HeaderHoldingALockOrAScalarLeaves::lockOsr, 4_000_000);
        row("scalar-osr", L2W61HeaderHoldingALockOrAScalarLeaves::scalarOsr, 4_000_000);
        warm(L2W61HeaderHoldingALockOrAScalarLeaves::lockAgain);
        row("lock-again", L2W61HeaderHoldingALockOrAScalarLeaves::lockAgain, 1000);
    }
}
