// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcf f3/f (2026-10-01): drive an owned-arguments call site's HAND-OFF on
// demand (docs/internal/gc/gcd-d10f-owned-args-handoff-replays-the-caller-unchecked-DONE-20260928.md).
//
// An owned-arguments site is a single-pass direct `invokestatic` whose caller
// commits nothing before the call (`caller` below: load the argument, call)
// into a callee body that owns its arguments (no handler, not synchronized:
// `Callee.park`). The site keeps no copy of the arguments, so when the
// callee's stashed frame is DECLINED by the call-site service it cannot
// re-run the callee; it hands the trap to the caller's caller instead and
// prints, under CRATONVM_DBG_DEOPT=1,
//   [cratonvm-deopt] declined callee trap dropped at an owned-arguments call site: ...
//   [cratonvm-deopt] owned-arguments call site: ... leaves as the caller's frameless deopt
// and, at exit (gcf f3/f, short form for a battery that cuts long middles),
//   [cratonvm-jitc] owned-args hand-offs (exact/unsound): <e>/<u>
//
// The decline is made on demand with a redefinition: thread T calls the
// compiled `caller`, whose baked call enters the compiled `park`, which parks
// on a latch; main redefines `Callee` (one constant changed) while T is
// parked, then releases it. A redefinition tells the running stale body to
// leave, and its stash names a body that is no longer published, which the
// call-site service does not resume as the current callee.
//
// HotSpot keeps running the OLD (obsolete) `park` for the active call: it
// prints `entries=1 value=1000003 PROBE-OK`. A replay of the callee from
// entry counts `entries=2` (the store before the trap ran twice); a re-run
// that reaches the NEW body prints `value=2000003`. Either is a divergence.
//
// The Instrumentation comes from GceE1fNotEntrantEscapeProbe's agent (the
// same jar, `-javaagent:.../GceE1fNotEntrantEscapeProbe.jar`), so no new jar
// is needed; that class must be on the class path.
//
// RUN: cratonvm|java -Xmx64m -javaagent:<GceE1fNotEntrantEscapeProbe.jar> -cp <classes> GcfF3fOwnedArgsHandoffProbe [warm]
// Expected (HotSpot 25 -XX:+UseSerialGC): entries=1 value=1000003 PROBE-OK
// Without the agent: `no agent PROBE-SKIP`, exit 2.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.concurrent.CountDownLatch;

public class GcfF3fOwnedArgsHandoffProbe {
    public static class Callee {
        static int entries;
        static volatile CountDownLatch in;
        static volatile CountDownLatch go;

        /// Owns its arguments: no handler, not synchronized. The store to
        /// `entries` is the side effect a replay would repeat.
        static int park(Object a) throws InterruptedException {
            entries++;
            CountDownLatch g = go;
            if (g != null && a != null) {
                in.countDown();
                g.await();
            }
            return 1000003;
        }
    }

    /// Commits nothing before the call, so the single-pass site may own its
    /// arguments (`owned_args_handoff_replays_exactly`).
    static int caller(Object a) throws InterruptedException {
        return Callee.park(a);
    }

    static volatile int result;

    /// Runs once, interpreted: enters the compiled `caller` through its own
    /// entry.
    static void once(Object a) throws InterruptedException {
        result = caller(a);
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
        Instrumentation inst = GceE1fNotEntrantEscapeProbe.inst;
        if (inst == null) {
            System.out.println("no agent PROBE-SKIP");
            System.exit(2);
        }
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        long sink = 0;
        // `park` first, so it is compiled before `caller` and the caller's
        // compile bakes the direct call.
        for (int i = 0; i < warm; i++) {
            sink += Callee.park(null);
        }
        for (int i = 0; i < warm; i++) {
            sink += caller(null);
        }
        Callee.entries = 0;
        Callee.in = new CountDownLatch(1);
        Callee.go = new CountDownLatch(1);
        final Object arg = new Object();
        final Throwable[] failed = new Throwable[1];
        Thread t = new Thread(() -> {
            try {
                once(arg);
            } catch (Throwable e) {
                failed[0] = e;
            }
        }, "handoff");
        t.start();
        Callee.in.await();
        byte[] bytes;
        try (InputStream s = GcfF3fOwnedArgsHandoffProbe.class
                .getResourceAsStream("GcfF3fOwnedArgsHandoffProbe$Callee.class")) {
            bytes = s.readAllBytes();
        }
        inst.redefineClasses(new ClassDefinition(Callee.class, patched(bytes)));
        Callee.go.countDown();
        t.join();
        if (failed[0] != null) {
            System.err.println("[probe] the parked call threw: " + failed[0]);
        }
        boolean ok = failed[0] == null && Callee.entries == 1 && result == 1000003
                && sink == 2L * warm * 1000003L;
        System.out.println("entries=" + Callee.entries + " value=" + result
                + (failed[0] != null ? " threw=" + failed[0].getClass().getName() : "")
                + (ok ? " PROBE-OK" : " PROBE-FAIL"));
        if (!ok) {
            System.exit(1);
        }
    }
}
