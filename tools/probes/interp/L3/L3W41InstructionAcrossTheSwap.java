// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L3: window 1 of
// docs/internal/fixed-bugs/interpreter-L3-obsolete-frame-moves-are-not-atomic-with-the-redefinition-FIXED-20261001.md
// made deterministic -- an instruction of an old body already IN FLIGHT when
// a renumbering redefinition of its class runs, with no dependence on timing
// or host load. The instruction is one that runs Java in its middle: the
// first `invokestatic` / `getstatic` / `new` naming a class not yet
// initialized, whose `<clinit>` blocks (in a latch, a blocking region) until
// the main thread has redefined the instruction's class with a class file
// whose constant pool javac numbered differently, and only then returns. The
// instruction then finishes. On HotSpot it finishes against the constant it
// decoded before the swap (resolution happens before initialization, and the
// redefinition's safepoint cannot split one bytecode); anything that re-reads
// the constant pool by the OLD index after the class initializer returns
// reads the NEW pool, where that index names another entry.
// docs/internal/fixed-bugs/interpreter-L2-a-spinning-obsolete-frame-sometimes-throws-internalerror-across-a-renumbering-redefinition-FIXED-20261007.md
// (the InternalError that page records is a constant-pool read of an old
// index in the new pool; this probe asks every instruction shape that can be
// suspended between its operand decode and its end, one row each).
//
// Rows, one redefinition each (edited, original, edited):
//   invoke -- `Gate1.value()` + `one` in `Tgt.viaInvoke`;
//   field  -- `Gate2.X` + `one` in `Tgt.viaField`;
//   new    -- `new Gate3().v` + `one` in `Tgt.viaNew`.
// Each prints the value (41 + 1) or the throwable, then the same method
// called again after the swap (the new body: 42 too, the edit changes only
// `tag`).
//
// HotSpot 25 prints (agent; the same with -Xint; run locally):
//     invoke value=42 threw=none again=42
//     field value=42 threw=none again=42
//     new value=42 threw=none again=42
//     fresh=tagB
// On a failure CratonVM also prints `<row> threw-message=` and `<row>
// threw-at` lines (HotSpot none).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W41InstructionAcrossTheSwap$Agent
//     Can-Redefine-Classes: true
// containing L3W41InstructionAcrossTheSwap*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W41InstructionAcrossTheSwap
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.util.concurrent.CountDownLatch;

public class L3W41InstructionAcrossTheSwap {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    static final CountDownLatch[] ENTERED = {new CountDownLatch(1), new CountDownLatch(1), new CountDownLatch(1)};
    static final CountDownLatch[] RELEASE = {new CountDownLatch(1), new CountDownLatch(1), new CountDownLatch(1)};

    static void hold(int row) {
        ENTERED[row].countDown();
        try {
            RELEASE[row].await();
        } catch (InterruptedException e) {
            throw new RuntimeException(e);
        }
    }

    public static class Gate1 {
        static {
            hold(0);
        }

        public static int value() {
            return 41;
        }
    }

    public static class Gate2 {
        static final int X;

        static {
            hold(1);
            X = 41;
        }
    }

    public static class Gate3 {
        static {
            hold(2);
        }

        int v = 41;
    }

    /** The class as first loaded. */
    public static class Tgt {
        static int one = 1;
        static int two = 2;

        public static int viaInvoke() {
            return Gate1.value() + one;
        }

        public static int viaField() {
            return Gate2.X + one;
        }

        public static int viaNew() {
            return new Gate3().v + one;
        }

        public static String tag() {
            return "tagA";
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        static int one = 1;
        static int two = 2;

        public static String tag() {
            String pad = "padB";
            if (two == pad.length()) {
                return "twoB";
            }
            StringBuilder b = new StringBuilder("tag");
            return b.append('B').toString();
        }

        public static int viaInvoke() {
            return Gate1.value() + one;
        }

        public static int viaField() {
            return Gate2.X + one;
        }

        public static int viaNew() {
            return new Gate3().v + one;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W41InstructionAcrossTheSwap.class
                .getResourceAsStream("L3W41InstructionAcrossTheSwap$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L3W41InstructionAcrossTheSwap$" + donor;
        String to = "L3W41InstructionAcrossTheSwap$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    interface Call {
        int run();
    }

    static void row(Instrumentation i, int index, String name, Call call, byte[] bytes)
            throws Exception {
        final int[] value = new int[1];
        final Throwable[] thrown = new Throwable[1];
        Thread worker = new Thread(() -> {
            try {
                value[0] = call.run();
            } catch (Throwable t) {
                thrown[0] = t;
            }
        }, "row-" + name);
        worker.setDaemon(true);
        worker.start();
        ENTERED[index].await();
        i.redefineClasses(new ClassDefinition(Tgt.class, bytes));
        RELEASE[index].countDown();
        worker.join(60_000);
        Throwable t = thrown[0];
        String again;
        try {
            again = String.valueOf(call.run());
        } catch (Throwable e) {
            again = e.getClass().getName();
        }
        System.out.println(name + " value=" + value[0] + " threw="
                + (t == null ? "none" : t.getClass().getName())
                + " again=" + again
                + (worker.isAlive() ? " (still running)" : ""));
        if (t != null) {
            System.out.println(name + " threw-message=" + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println(name + " threw-at " + e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] original = bytesOf("Tgt");
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.tag();
        row(i, 0, "invoke", Tgt::viaInvoke, edited);
        row(i, 1, "field", Tgt::viaField, original);
        row(i, 2, "new", Tgt::viaNew, edited);
        System.out.println("fresh=" + Tgt.tag());
    }
}
