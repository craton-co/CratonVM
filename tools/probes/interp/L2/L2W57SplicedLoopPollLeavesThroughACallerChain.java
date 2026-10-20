// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L2 -- the positive control of
// docs/internal/fixed-bugs/interpreter-L2-proposal-optimizing-polls-in-a-spliced-loop-leave-through-a-caller-chain-FIXED-20261021.md
// (`CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS` for OSR-door compiles,
// `CRATONVM_JIT_IR_ENTRY_SPLICED_HEADER_POLL_EXITS` for method-entry ones):
// an optimizing body that SPLICED a small callee `inner*` with a loop of its
// own is spinning in that loop when a class the splice copied is
// retransformed. HotSpot's deopt resumes `inner*` in the interpreter where
// it is, so every later iteration of its loop reads the NEW
// `Callee*.value()`.
//
// `inner*(n, parkAt)` sums `Callee*.value()` over `n` iterations; at
// iteration `parkAt` it spins on a volatile (a second loop, also inside the
// splice) until main lets it go. It writes no field (a store fences a splice
// off), so its caller raises the park flag before the parking call. Main
// retransforms the row's `Callee*` (12345 becomes 12346) while it spins.
// Each caller calls `inner*` in a loop (a hot site, so it is spliced). Rows:
//   entry        `entryE(2, n, 1)`, reached through reflection only (no
//                compiled caller splices it), warmed by 20,000 calls: its
//                METHOD-ENTRY optimizing body splices `innerE`;
//   osr          `osrO(3000, n, 2999)`, called once: the OSR door's
//                optimizing body splices `innerO`;
//   entry-again  the first row again, on its own classes, last.
// `new-reads-after` counts the iterations after the park that read the
// retransformed value (iterations parkAt+1 .. n-1: 499).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 with the agent, default flags; the same
// with -Xint)
// entry new-reads-after=499
// osr new-reads-after=499
// entry-again new-reads-after=499
// HOTSPOT_EXPECTED_END
// Without the agent: `no agent`.
//
// CratonVM, measured on the lane's debug build (Windows): `--nojit` prints
// HotSpot's lines. JIT on, switches OFF (the base `a12cd826d4`): all three rows
// print `new-reads-after=0` (`[ir-mode-exit-refused] shape:spliced-header`
// under CRATONVM_DBG_JITC=1: the spliced loop's poll had no exit, so the
// activation ran the rest of `inner*` compiled, on the old
// `Callee*.value()`). Switches ON: HotSpot's lines (3 runs default, 1
// `--compatible`).
//   POSITIVE CONTROL, under `CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1`:
//     `[ir-mode-exit-given] spliced-header osr-door scope=...innerO:(II)J@24
//     caller=...osrO:(III)J@35 depth=2` (and `method-entry` for `entryE` /
//     `entryA`) at the compile; then `withdrawn body told to leave:
//     ...entryE:(III)J verdict=0x7`, `sink=execute-first-call-tierup ...
//     stash=...innerE:(II)J bci=24` and `PRECISE resume of a 2-frame inlined
//     chain: ...entryE@38 -> ...innerE@24` (the OSR row: `sink=osr-exit` and
//     `OSR-exit TRANSFER ...osrO(III)J entry_pc=5 resume_bci=24`).
//   A row whose body was still the single-pass one (no splice of `inner*`)
//   is not discriminating: it calls `inner*` for real and reads the new
//   value either way.
// RACY: the park handshake only (deterministic otherwise). Run it 5 times
// per mode.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W57SplicedLoopPollLeavesThroughACallerChain$Agent
//     Can-Retransform-Classes: true
// containing L2W57SplicedLoopPollLeavesThroughACallerChain*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W57SplicedLoopPollLeavesThroughACallerChain
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;

public class L2W57SplicedLoopPollLeavesThroughACallerChain {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class CalleeE {
        static int value() {
            return 12345;
        }
    }

    public static class CalleeO {
        static int value() {
            return 12345;
        }
    }

    public static class CalleeA {
        static int value() {
            return 12345;
        }
    }

    static final String PREFIX = "L2W57SplicedLoopPollLeavesThroughACallerChain$Callee";

    /**
     * On a RETRANSFORM of a callee class (never at the initial load), rewrites
     * `sipush 12345; ireturn` (11 30 39 AC) to answer 12346.
     */
    static final class Bump implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || className == null || !className.startsWith(PREFIX)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 3 < out.length; i++) {
                if (out[i] == 0x11 && out[i + 1] == 0x30 && out[i + 2] == 0x39
                        && out[i + 3] == (byte) 0xAC) {
                    out[i + 2] = 0x3A;
                }
            }
            return out;
        }
    }

    static final int OLD = 12345;
    static final int N = 1000;
    static final int PARK_AT = 500;
    static volatile boolean inSpin;
    static volatile boolean go;

    /**
     * Spliced into `entryE`: a loop of its own, and a park loop. It writes
     * no field (a store inside a splice fences the splice off), so the caller
     * raises the park flag before the call.
     */
    static long innerE(int n, int parkAt) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += CalleeE.value();
            if (k == parkAt) {
                while (!go) {
                }
            }
        }
        return s;
    }

    /** Spliced into `osrO`. */
    static long innerO(int n, int parkAt) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += CalleeO.value();
            if (k == parkAt) {
                while (!go) {
                }
            }
        }
        return s;
    }

    /** Spliced into `entryA`. */
    static long innerA(int n, int parkAt) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += CalleeA.value();
            if (k == parkAt) {
                while (!go) {
                }
            }
        }
        return s;
    }

    /**
     * Reached through reflection only: a method-entry body of its own, whose
     * call of `innerE` sits in a loop (a hot site, so it is spliced). Round
     * `parkRound` parks at iteration `PARK_AT`.
     */
    public static long entryE(int rounds, int n, int parkRound) {
        long last = 0;
        for (int r = 0; r < rounds; r++) {
            if (r == parkRound) {
                inSpin = true;
            }
            last = innerE(n, r == parkRound ? PARK_AT : -1);
        }
        return last;
    }

    /** Reached through reflection only, as `entryE`. */
    public static long entryA(int rounds, int n, int parkRound) {
        long last = 0;
        for (int r = 0; r < rounds; r++) {
            if (r == parkRound) {
                inSpin = true;
            }
            last = innerA(n, r == parkRound ? PARK_AT : -1);
        }
        return last;
    }

    /** Called once: its loop is entered through the OSR door. */
    static long osrO(int rounds, int n, int parkRound) {
        long last = 0;
        for (int r = 0; r < rounds; r++) {
            if (r == parkRound) {
                inSpin = true;
            }
            last = innerO(n, r == parkRound ? PARK_AT : -1);
        }
        return last;
    }

    interface Body {
        long run() throws Exception;
    }

    static String row(Instrumentation i, Body body, Class<?> callee) throws Exception {
        inSpin = false;
        go = false;
        long[] sum = {-1};
        Thread worker = new Thread(() -> {
            try {
                sum[0] = body.run();
            } catch (Exception e) {
                throw new RuntimeException(e);
            }
        });
        worker.start();
        while (!inSpin) {
            Thread.sleep(1);
        }
        Thread.sleep(50);
        i.retransformClasses(callee);
        go = true;
        worker.join();
        return "new-reads-after=" + (sum[0] - (long) N * OLD);
    }

    static Body entryRow(Method m) {
        return () -> {
            long warm = 0;
            for (int k = 0; k < 20_000; k++) {
                warm += (Long) m.invoke(null, 2, 50, -1);
            }
            if (warm != 20_000L * 50 * OLD) {
                throw new IllegalStateException("warm-up sum " + warm);
            }
            return (Long) m.invoke(null, 2, N, 1);
        };
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        i.addTransformer(new Bump(), true);
        Class<?> self = L2W57SplicedLoopPollLeavesThroughACallerChain.class;
        Method entryE = self.getMethod("entryE", int.class, int.class, int.class);
        Method entryA = self.getMethod("entryA", int.class, int.class, int.class);
        System.out.println("entry " + row(i, entryRow(entryE), CalleeE.class));
        System.out.println("osr " + row(i, () -> osrO(3_000, N, 2_999), CalleeO.class));
        System.out.println("entry-again " + row(i, entryRow(entryA), CalleeA.class));
    }
}
