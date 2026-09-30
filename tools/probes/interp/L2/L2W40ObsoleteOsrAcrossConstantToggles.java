// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L2: the background worker compiles OSR
// tasks of a redefined class again (`background_compile_task`), which waves
// 38-39 declined
// (docs/internal/fixed-bugs/interpreter-L2-an-obsolete-activation-enters-an-osr-body-of-the-new-bytecode-FIXED-20261004.md).
// This is the stress version of `L2W38RedefinedClassCompiles`: four
// generations of one hot loop run at once, and every redefinition changes
// ONLY the loop's `CONSTANT_Integer` (patched in place; the bytecode is
// byte-identical in every version), so no byte comparison can tell the
// versions apart and only the constant-pool generation can.
//
//   v0: acc += 1000003   thread t0 starts, gets hot (OSR)
//   v1: acc += 2000003   t0 is obsolete; t1 starts, gets hot
//   v2: acc += 1000003   (the original bytes again) t1 is obsolete; t2 starts
//   v3: acc += 3000003   t2 is obsolete; t3 starts
//
// Each activation must keep adding the constant of the version it started
// in (HotSpot: an EMCP method keeps its own constant pool), whatever OSR
// bodies the later versions published. t0 in v2 reads the same VALUE as the
// current version, so OSR of the current body would be harmless for it; t1
// and t2 are the rows a generation mix-up turns false. Each row checks the
// wrapped `int` sum against the `long` iteration count.
//
// HotSpot 25 (agent; the same with -Xint; local JDK 25.0.3, 3/3 runs):
//   t0 v0 constant=true
//   t1 v1 constant=true
//   t2 v2 constant=true
//   t3 v3 constant=true
// Expected on CratonVM in every mode (default, --nojit, --compatible, and
// with CRATONVM_BG_COMPILE=0): the same four lines.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W40ObsoleteOsrAcrossConstantToggles$Agent
//     Can-Redefine-Classes: true
// containing L2W40ObsoleteOsrAcrossConstantToggles*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W40ObsoleteOsrAcrossConstantToggles
// Without the agent both VMs print "no agent".
//
// Positive control (default mode), CRATONVM_DBG_JITC=1 on stderr:
//     grep 'bg-compile L2W40ObsoleteOsrAcrossConstantToggles$Hot.spin(\[J)V.*osr_bci=.*redefined-class'
// (the worker now compiles the redefined class's OSR task; before wave 40 the
// line was `bg-compile declined ... osr_bci=...: redefined class`), and
//     grep 'OSR entry after a redefinition: .*Hot.spin'
// names each body a Hot.spin activation entered after a redefinition, with
// `frame_stamp=`, `body_stamp=` and `predates=` (true: an activation moved
// across a redefinition, admitted only as an EMCP one). An entry the
// generation guard refused prints `OSR entry REFUSED (the body's
// constant-pool generation is not the frame's)`.
// `CRATONVM_JIT_BG_DECLINE_REDEFINED=1` restores the wave-37 policy (no
// `bg-compile ... redefined-class` line for an OSR task).
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W40ObsoleteOsrAcrossConstantToggles {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Hot {
        public static volatile boolean stop;

        /// `out[0]` = iterations (a `long`), `out[1]` = the wrapped `int` sum.
        public static void spin(long[] out) {
            int acc = 0;
            long n = 0;
            while (!stop) {
                acc += 1000003;
                n++;
            }
            out[0] = n;
            out[1] = acc;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W40ObsoleteOsrAcrossConstantToggles.class
                .getResourceAsStream("L2W40ObsoleteOsrAcrossConstantToggles$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /// `bytes` with its one CONSTANT_Integer 1000003 turned into `k`.
    static byte[] withConstant(byte[] bytes, int k) {
        byte[] b = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 5 <= b.length; i++) {
            if (b[i] == 3 && b[i + 1] == 0x00 && b[i + 2] == 0x0F && b[i + 3] == 0x42
                    && b[i + 4] == 0x43) {
                b[i + 1] = (byte) (k >>> 24);
                b[i + 2] = (byte) (k >>> 16);
                b[i + 3] = (byte) (k >>> 8);
                b[i + 4] = (byte) k;
                hits++;
            }
        }
        if (hits != 1) {
            throw new AssertionError("constant found " + hits + " times");
        }
        return b;
    }

    /// The sum is `n * k` in wrapping `int` arithmetic.
    static boolean matches(long[] out, int k) {
        return out[0] > 0 && (int) out[1] == (int) (out[0] * k);
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] original = bytesOf("Hot");
        int[] constants = {1000003, 2000003, 1000003, 3000003};
        long[][] outs = new long[constants.length][2];
        Thread[] threads = new Thread[constants.length];
        for (int v = 0; v < constants.length; v++) {
            if (v > 0) {
                i.redefineClasses(new ClassDefinition(Hot.class,
                        withConstant(original, constants[v])));
            }
            long[] out = outs[v];
            threads[v] = new Thread(() -> Hot.spin(out), "t" + v);
            threads[v].start();
            Thread.sleep(400);
        }
        Thread.sleep(400);
        Hot.stop = true;
        for (Thread t : threads) {
            t.join(60_000);
        }
        for (int v = 0; v < constants.length; v++) {
            System.out.println("t" + v + " v" + v + " constant=" + matches(outs[v], constants[v]));
        }
    }
}
