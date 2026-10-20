// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gce e1/f, made decisive by gce e2/f (2026-09-29): an exception that escaped
// a NOT-ENTRANT re-dispatch must not be offered to the callee's own handlers a
// second time
// (docs/internal/gc/gcd-d10f-not-entrant-redispatch-exception-reruns-the-callee-handler-DONE-20260928.md).
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
// argument is the warm-up count (default 200000); the optional second is the
// number of failing calls after the redefinition (default 1, the line above;
// with N the line reads `caught=N`).
//
// gcf f1/f (2026-10-01), the decisive row: `200000 20` under
// CRATONVM_C2_SUPERSEDE=0 CRATONVM_JIT_GUARDED_VIRTUAL_INLINE=0 keeps `caller`
// on its single-pass body with the inline cache that names the stale `m`
// (ve2 f-5/f-6: on Linux the optimizing tier superseded `caller` before the
// last call, which then went through the Rust door, and neither arm
// re-dispatched). HotSpot prints `redefined=true mark=2000003 logs=0
// caught=20 PROBE-OK`.
//
// gcf f3/f (2026-10-01): vf1 f-3 ran that row 30 times and no failing call
// reached the stub (`not-entrant pass: candidates=2 patched=2`, 0
// `re-dispatching` lines; the `=0` control printed PROBE-OK). A redefinition
// clears inline caches and cells; only a caller that BAKED a direct call to
// the stale entry before the redefinition lands on the patched stub. The
// third argument `static` selects that shape: `callerStatic` calls
// `Callee.sm` (m's body, static) with `invokestatic`, and `sm` is warmed
// before `callerStatic` so the caller's compile bakes the call. Row:
// `GceE1fNotEntrantEscapeProbe 200000 20 static`; HotSpot prints the same
// line as the interface shape (`redefined=true mark=2000003 logs=0 caught=20
// PROBE-OK`).
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

        /// gcf f3/f: `m`'s body as a static method, for the `static` shape
        /// (a BAKED direct call; see `callerStatic`). The same constant, so the
        /// class keeps one `CONSTANT_Integer 1000003` for `patched` to find.
        public static void sm(boolean fail) throws IOException {
            mark = 1000003;
            try {
                a();
            } catch (IOException e) {
                logs++;
            }
            if (fail) {
                throw new IOException("escapes sm");
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

    /// gcf f3/f: the `static` shape's caller. `invokestatic Callee.sm` is a
    /// statically bound call, which a compiled caller BAKES as a direct call
    /// to `sm`'s compiled entry once `sm` is compiled (it is warmed first,
    /// see `main`). A redefinition clears inline caches and cells but cannot
    /// rewrite a baked call: that call is exactly what lands on the
    /// not-entrant stub patched over the stale entry (`JitCache::
    /// make_not_entrant`: "no inline cache, cell or baked caller published
    /// later takes it" -- only a caller baked BEFORE does). vf1 f-3 measured
    /// the interface shape reaching no stub on Linux (`patched=2`, 0
    /// `re-dispatching` lines in 30 rows): its call site is an inline cache.
    static int callerStatic(boolean fail) {
        try {
            Callee.sm(fail);
            return 0;
        } catch (IOException e) {
            return 1;
        }
    }

    static int afterRedefinitionStatic(int calls) {
        int caught = 0;
        for (int k = 0; k < calls; k++) {
            caught += callerStatic(true);
        }
        return caught;
    }

    /// gcf f1/f: the failing calls after the redefinition, from a method that
    /// runs once (interpreted, too short to OSR), so every call enters the
    /// compiled `caller` through its own entry and reaches `m` through that
    /// body's inline cache -- not through a `main` OSR body that may have
    /// inlined `caller`. Each call that reaches the not-entrant stub is one
    /// more chance for the defect (`logs` counts every double handler run).
    static int afterRedefinition(int calls) {
        int caught = 0;
        for (int k = 0; k < calls; k++) {
            caught += caller(TASK, true);
        }
        return caught;
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
        int calls = args.length > 1 ? Integer.parseInt(args[1]) : 1;
        // gcf f3/f: the optional third argument `static` selects the baked
        // direct-call shape; anything else (or none) the interface shape.
        boolean statik = args.length > 2 && args[2].equals("static");
        int sink = 0;
        if (statik) {
            // `sm` first, so it is compiled before `callerStatic` is, and the
            // caller's compile bakes a direct call to its entry.
            for (int i = 0; i < warm; i++) {
                Callee.sm(false);
            }
            for (int i = 0; i < warm; i++) {
                sink += callerStatic(false);
            }
        } else {
            for (int i = 0; i < warm; i++) {
                sink += caller(TASK, false);
            }
        }
        byte[] bytes;
        try (InputStream in = GceE1fNotEntrantEscapeProbe.class
                .getResourceAsStream("GceE1fNotEntrantEscapeProbe$Callee.class")) {
            bytes = in.readAllBytes();
        }
        inst.redefineClasses(new ClassDefinition(Callee.class, patched(bytes)));
        int caught = statik ? afterRedefinitionStatic(calls) : afterRedefinition(calls);
        boolean ok = Callee.mark == 2000003 && Callee.logs == 0 && caught == calls && sink == 0;
        System.out.println("redefined=true mark=" + Callee.mark + " logs=" + Callee.logs
                + " caught=" + caught + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        if (!ok) {
            System.exit(1);
        }
    }
}
