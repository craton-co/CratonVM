// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gce e1/f, made decisive by gce e2/f (2026-09-29): an exception that escaped
// a NOT-ENTRANT re-dispatch must not be offered to the callee's own handlers a
// second time
// (docs/known-issues/gc/gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-20260928.md).
//
// `caller` is compiled with a monomorphic interface call (`Task.m`) whose
// inline cache holds the compiled entry of `Callee.m`. The agent then
// redefines `Callee` with ONE constant changed (`m` stores 1000003 into `mark`
// before it does anything else; the redefined class stores 2000003), which
// patches every stale compiled entry of `m` with its not-entrant stub. The
// next call from the compiled `caller` must run the NEW `m`: `a()` does not
// throw, the `throw` after the `try` escapes `m` (no row covers it), and the
// exception comes back to `caller`, which catches it.
//
// Reaching the stub depends on which `caller` body is still running at the
// last call (the single-pass one, whose IC word names the stale `m`):
// measured on the base `adb9178bc` (Windows, Generational), the re-dispatch
// happened in 5 of 8 runs with CRATONVM_DBG_JITC=1 and printed `logs=1
// PROBE-FAIL` every time it did; in 0 of 12 runs without that switch. Run the
// judged row with CRATONVM_DBG_JITC=1 CRATONVM_DBG_DEOPT=1 and count the
// `re-dispatching` lines on stderr (>= 1 makes the row decisive).
//
// Three answers on ONE stdout line, each decisive:
//   mark=2000003  the call ran the redefined `m` (1000003: a stale compiled
//                 body of the OLD class ran after the redefinition -- a
//                 separate defect, JVMS 5.4.3 / JVMTI RedefineClasses);
//   logs=0        the handler that does not cover the throw never ran (1: the
//                 suspected defect -- the call site's service offered the
//                 escaped exception to `m`'s table again, pc unknown);
//   caught=1      the exception reached `caller`.
// A run WITHOUT the agent is not a pass: it prints `no agent PROBE-SKIP` and
// exits 2, so a row that forgot `-javaagent` reads DIFF, never SAME.
//
// BUILD (the manifest is checked in beside this file):
//     javac -d out tools/probes/GceE1fNotEntrantEscapeProbe.java
//     jar cfm out/GceE1fNotEntrantEscapeProbe.jar tools/probes/GceE1fNotEntrantEscapeProbe.mf -C out .
// RUN:
//     java|cratonvm [-XX:+UseGenerationalGC|-XX:+UseG1GC|-XX:+UseZGC] -Xmx64m \
//         -javaagent:out/GceE1fNotEntrantEscapeProbe.jar -cp out/GceE1fNotEntrantEscapeProbe.jar \
//         GceE1fNotEntrantEscapeProbe
// Expected (HotSpot 25 -XX:+UseSerialGC, and -Xint):
//     redefined=true mark=2000003 logs=0 caught=1 PROBE-OK
// A/B arm: CRATONVM_JIT_NOT_ENTRANT_ESCAPE=0. Engagement, on stderr under
// CRATONVM_DBG_DEOPT=1: `not-entrant entry: re-dispatching
// GceE1fNotEntrantEscapeProbe$Callee.m` (absent = the call did not reach the
// stub; then only `mark` and `caught` were judged). The optional first
// argument is the warm-up count (default 200000).
import java.io.IOException;
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class GceE1fNotEntrantEscapeProbe {
    static volatile Instrumentation inst;
    static int warm = 200_000;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public interface Task {
        void m(boolean fail) throws IOException;
    }

    public static class Callee implements Task {
        static int logs;
        static int mark;

        static void a() throws IOException {
            // Never throws; the `try` below covers this call only.
        }

        public void m(boolean fail) throws IOException {
            mark = 1000003;
            try {
                a();
            } catch (IOException e) {
                logs++;
            }
            if (fail) {
                throw new IOException("escapes m");
            }
        }
    }

    static final Task TASK = new Callee();

    static int caller(Task t, boolean fail) {
        try {
            t.m(fail);
            return 0;
        } catch (IOException e) {
            return 1;
        }
    }

    /// `bytes` with its one CONSTANT_Integer 1000003 turned into 2000003.
    static byte[] patched(byte[] bytes) {
        byte[] b = bytes.clone();
        int hits = 0;
        for (int i = 0; i + 5 <= b.length; i++) {
            if (b[i] == 3 && b[i + 1] == 0x00 && b[i + 2] == 0x0F && b[i + 3] == 0x42
                    && b[i + 4] == 0x43) {
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

    public static void main(String[] args) throws Exception {
        if (inst == null) {
            System.out.println("no agent PROBE-SKIP");
            System.exit(2);
        }
        if (args.length > 0) {
            warm = Integer.parseInt(args[0]);
        }
        int sink = 0;
        for (int i = 0; i < warm; i++) {
            sink += caller(TASK, false);
        }
        byte[] bytes;
        try (InputStream in = GceE1fNotEntrantEscapeProbe.class
                .getResourceAsStream("GceE1fNotEntrantEscapeProbe$Callee.class")) {
            bytes = in.readAllBytes();
        }
        inst.redefineClasses(new ClassDefinition(Callee.class, patched(bytes)));
        int caught = caller(TASK, true);
        boolean ok = Callee.mark == 2000003 && Callee.logs == 0 && caught == 1 && sink == 0;
        System.out.println("redefined=true mark=" + Callee.mark + " logs=" + Callee.logs
                + " caught=" + caught + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        if (!ok) {
            System.exit(1);
        }
    }
}
