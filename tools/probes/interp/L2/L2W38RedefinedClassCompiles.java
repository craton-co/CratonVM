// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: item 5 of the wave-37 compile-door
// review. The background compile worker declined every task of a class that
// had EVER been redefined (`background_compile_task`,
// `class_id_or_name_was_redefined`), and so did the upgrade door
// (`try_jit_upgrade_with_gate`, `gate.generation > 0`), while the eager
// first-call door, the callee door and `try_osr` compiled such a class. Since
// wave 38 both compile a redefined class at METHOD ENTRY; the worker's OSR
// tasks of a redefined class stay declined by default (see below).
//
// This probe is the correctness guard: the hazard is a frame running the
// REPLACED body entering code compiled from the new one. Thread `old` runs
// `Hot.spin(out)` (a hot `acc += 1000003` loop), and while it runs `main`
// redefines `Hot` with the literal changed to 2000003 (the class file's
// CONSTANT_Integer patched in place; the bytecode is byte-identical). Thread
// `old`'s activation is then obsolete and must keep adding 1000003; thread
// `fresh`, started after the redefinition, runs the new body and must add
// 2000003. Each activation reports its iteration count (a `long`) and its
// wrapped `int` sum; each row checks sum == (int) (count * K). `main` then
// calls `Hot.k()` 50,000 times (a method-entry compile of the new body).
//
// HotSpot 25 prints (agent; the same with -Xint and TieredStopAtLevel=1; 4/4
// runs locally):
//   obsolete activation: old constant=true
//   fresh activation: new constant=true
//   after: k=2000003
// The first version of this probe counted iterations in an `int` and asked
// `n > 0`; HotSpot 25.0.4 on the Linux host printed `new constant=false` 3/3
// with it (and wave 37 too): C2's loop passes 2^31 iterations within the
// fresh thread's 700 ms, so `n` went negative. The `long` count removes that.
//
// Wave-38 host run of the first version (default, JIT): `old constant=false`
// -- the obsolete activation read the NEW constant -- with the worker
// compiling OSR tasks of the redefined class; `old constant=true` with
// `CRATONVM_JIT_BG_DECLINE_REDEFINED=1` and on wave 37. Hence the OSR decline
// (`docs/internal/fixed-bugs/interpreter-L2-an-obsolete-activation-enters-an-osr-body-of-the-new-bytecode-FIXED-20261004.md`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W38RedefinedClassCompiles$Agent
//     Can-Redefine-Classes: true
// containing L2W38RedefinedClassCompiles*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W38RedefinedClassCompiles
// Without the agent both VMs print "no agent". Expected on CratonVM (every
// mode): HotSpot's three lines. The A/B arm:
//     CRATONVM_JIT_BG_DECLINE_REDEFINED=1   HotSpot's lines (wave-37 policy)
// (Waves 38-39 also had an opt-in that admitted the worker's OSR tasks of a
// redefined class, then declined by default; the host matched HotSpot 30/30
// with it, and wave 40 admits them by default. The stress version is
// L2W40ObsoleteOsrAcrossConstantToggles.)
//
// Positive control (default mode), with CRATONVM_DBG_JITC=1 on stderr:
//     grep 'bg-compile L2W38RedefinedClassCompiles$Hot.k()I.*redefined-class'
// (the method-entry compile of the redefined class, new in wave 38) and
//     grep 'bg-compile L2W38RedefinedClassCompiles$Hot.spin(\[J)V.*osr_bci=.*redefined-class'
// (the OSR task of the redefined class, compiled since wave 40; waves 38-39
// printed `bg-compile declined ... osr_bci=` here).
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W38RedefinedClassCompiles {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Hot {
        public static volatile boolean stop;

        /// `out[0]` = iterations (a `long`: a compiled loop can pass 2^31 in
        /// the probe's 700 ms), `out[1]` = the wrapped `int` sum.
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

        public static int k() {
            return 1000003;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W38RedefinedClassCompiles.class
                .getResourceAsStream("L2W38RedefinedClassCompiles$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /// `bytes` with every CONSTANT_Integer 1000003 turned into 2000003.
    static byte[] patched(byte[] bytes) {
        byte[] b = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 5 <= b.length; i++) {
            if (b[i] == 3 && b[i + 1] == 0x00 && b[i + 2] == 0x0F && b[i + 3] == 0x42
                    && b[i + 4] == 0x43) {
                b[i + 1] = 0x00;
                b[i + 2] = 0x1E;
                b[i + 3] = (byte) 0x84;
                b[i + 4] = (byte) 0x83;
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
        byte[] redefined = patched(bytesOf("Hot"));
        long[] oldOut = new long[2];
        long[] freshOut = new long[2];
        Thread old = new Thread(() -> Hot.spin(oldOut), "old");
        old.start();
        Thread.sleep(300);
        i.redefineClasses(new ClassDefinition(Hot.class, redefined));
        Thread fresh = new Thread(() -> Hot.spin(freshOut), "fresh");
        fresh.start();
        Thread.sleep(700);
        Hot.stop = true;
        old.join(60_000);
        fresh.join(60_000);
        System.out.println("obsolete activation: old constant=" + matches(oldOut, 1000003));
        System.out.println("fresh activation: new constant=" + matches(freshOut, 2000003));
        int k = 0;
        for (int c = 0; c < 50_000; c++) {
            k = Hot.k();
        }
        System.out.println("after: k=" + k);
    }
}
