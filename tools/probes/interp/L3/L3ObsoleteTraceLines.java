// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 22, lane L3: line numbers of stack frames that
// were captured, or are still running, in a method body a JVMTI
// RetransformClasses replaced (JEP 109 obsolete methods), read AFTER the
// retransform. The transformer only moves every LineNumberTable line of the
// two target classes down by 100, so nothing but the lines differs between
// the versions.
//
//   early    -- a Throwable built in Quiet.make() before the retransform,
//               while no frame of Quiet runs; its trace is read after.
//   held     -- the same, for Busy.make(), while another thread is parked in
//               Busy.parked (so the old version of Busy stays in use).
//   blocked  -- Thread.getStackTrace() of that parked thread, taken after the
//               retransform while it is still parked in the old body
//               (docs/internal/fixed-bugs/
//               interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md,
//               window 4).
//   inside   -- the parked frame's own trace once it runs again.
//   fresh    -- a Throwable built by a call made after the retransform.
//
// HotSpot 25 prints (with the agent; -Xint too):
//     early=own held=69 blocked=75 inside=76 fresh=169
// HotSpot resolves a backtrace element's line from the method of the class
// VERSION it was captured in. `early`'s version no frame runs, so HotSpot
// drops it at the retransform and answers -1; CratonVM (interpreter round i1
// wave 22, lane L3) keeps that version's line, 63. The probe prints "own"
// for either; the new table's line (163) is the bug.
// CratonVM before wave 22 (read from the code, not run): early=163 held=169
// blocked=175 inside=76 fresh=169 -- a Throwable's trace and a blocked
// thread's published snapshot were resolved when read, from the class's
// table as it was THEN.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3ObsoleteTraceLines$Agent
//     Can-Retransform-Classes: true
// containing L3ObsoleteTraceLines*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3ObsoleteTraceLines
// Without the agent both VMs print
//     no agent
//     early=own held=69 blocked=75 inside=76 fresh=69
// (The line numbers are this file's own: keep the layout above Busy.parked
// unchanged, or update them.)
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.nio.charset.StandardCharsets;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3ObsoleteTraceLines {
    static volatile Instrumentation inst;
    static int transforms;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Quiet {
        public static Throwable make() {
            return new Throwable();
        }
    }

    public static class Busy {
        public static Throwable make() {
            return new Throwable();
        }

        public static int parked(CountDownLatch parked, CountDownLatch go)
                throws InterruptedException {
            parked.countDown();
            go.await();
            return new Throwable().getStackTrace()[0].getLineNumber();
        }
    }

    static final class Shift implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!className.startsWith("L3ObsoleteTraceLines$")
                    || !(className.endsWith("$Quiet") || className.endsWith("$Busy"))) {
                return null;
            }
            transforms++;
            byte[] out = bytes.clone();
            shiftLines(out, 100);
            return out;
        }
    }

    static int u2(byte[] b, int p) {
        return ((b[p] & 0xff) << 8) | (b[p + 1] & 0xff);
    }

    static int u4(byte[] b, int p) {
        return (u2(b, p) << 16) | u2(b, p + 2);
    }

    /** Add `delta` to every LineNumberTable line of the class file `b`. */
    static void shiftLines(byte[] b, int delta) {
        int p = 8;
        int count = u2(b, p);
        p += 2;
        int code = -1;
        int lines = -1;
        for (int i = 1; i < count; i++) {
            int tag = b[p] & 0xff;
            switch (tag) {
                case 1: {
                    int len = u2(b, p + 1);
                    String s = new String(b, p + 3, len, StandardCharsets.UTF_8);
                    if (s.equals("Code")) {
                        code = i;
                    } else if (s.equals("LineNumberTable")) {
                        lines = i;
                    }
                    p += 3 + len;
                    break;
                }
                case 3: case 4: case 9: case 10: case 11: case 12: case 17: case 18:
                    p += 5;
                    break;
                case 5: case 6:
                    p += 9;
                    i++;
                    break;
                case 7: case 8: case 16: case 19: case 20:
                    p += 3;
                    break;
                case 15:
                    p += 4;
                    break;
                default:
                    throw new IllegalStateException("constant tag " + tag);
            }
        }
        p += 6; // access_flags, this_class, super_class
        p += 2 + 2 * u2(b, p); // interfaces
        for (int kind = 0; kind < 2; kind++) { // fields, then methods
            int members = u2(b, p);
            p += 2;
            for (int m = 0; m < members; m++) {
                int attributes = u2(b, p + 6);
                p += 8;
                for (int a = 0; a < attributes; a++) {
                    int name = u2(b, p);
                    int len = u4(b, p + 2);
                    if (name == code) {
                        int q = p + 6 + 4; // max_stack, max_locals
                        q += 4 + u4(b, q); // code
                        q += 2 + 8 * u2(b, q); // exception table
                        int inner = u2(b, q);
                        q += 2;
                        for (int c = 0; c < inner; c++) {
                            int innerName = u2(b, q);
                            int innerLen = u4(b, q + 2);
                            if (innerName == lines) {
                                int entries = u2(b, q + 6);
                                for (int e = 0; e < entries; e++) {
                                    int at = q + 8 + 4 * e + 2;
                                    int line = u2(b, at) + delta;
                                    b[at] = (byte) (line >> 8);
                                    b[at + 1] = (byte) line;
                                }
                            }
                            q += 6 + innerLen;
                        }
                    }
                    p += 6 + len;
                }
            }
        }
    }

    static int line(Throwable t) {
        return t.getStackTrace()[0].getLineNumber();
    }

    public static void main(String[] args) throws Exception {
        if (inst == null) {
            System.out.println("no agent");
        }
        Throwable early = Quiet.make();
        // The line Quiet.make's statement has before the retransform, read
        // from ANOTHER Throwable (reading `early` now would fix its trace).
        int quietLine = line(Quiet.make());

        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        int[] inside = new int[1];
        Thread worker = new Thread(() -> {
            try {
                inside[0] = Busy.parked(parked, go);
            } catch (Throwable t) {
                inside[0] = -99;
            }
        });
        worker.start();
        parked.await();
        Throwable held = Busy.make();
        // Wait until the worker is really parked in go.await().
        while (worker.getState() != Thread.State.WAITING) {
            Thread.onSpinWait();
        }
        if (inst != null) {
            // Added only now, so the classes were loaded untransformed.
            inst.addTransformer(new Shift(), true);
            inst.retransformClasses(Quiet.class, Busy.class);
        }
        int blocked = -98;
        for (StackTraceElement e : worker.getStackTrace()) {
            if (e.getMethodName().equals("parked")) {
                blocked = e.getLineNumber();
                break;
            }
        }
        go.countDown();
        worker.join();
        int fresh = line(Busy.make());
        // HotSpot answers -1 for `early` (the class version it was captured
        // in is gone); CratonVM keeps that version's line. Either is "own";
        // the new table's line is the bug.
        int earlyLine = line(early);
        String earlyText = earlyLine == quietLine || earlyLine == -1
                ? "own" : String.valueOf(earlyLine);
        System.out.println("early=" + earlyText + " held=" + line(held) + " blocked=" + blocked
                + " inside=" + inside[0] + " fresh=" + fresh);
    }
}
