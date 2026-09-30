// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 46, lane L2: two long-running METHOD-ENTRY
// activations of one class keep running while the class is redefined ONCE
// with a renumbered constant pool (a class recompiled by javac after an
// edit). JEP 109: each running activation keeps its own bytecode and its own
// constants, whichever way the VM runs the rest of it.
// docs/internal/fixed-bugs/interpreter-L2-proposal-method-entry-obsolete-bodies-leave-through-the-rebuild-sinks-FIXED-20261010.md
//
// Lane "door": `Tgt.spin` called from an interpreted frame (the worker's
// lambda), so its exit is resumed by an interpreter door's sink
// (`deopt_resume::real_frame_deopt_resume_or_throw_and_despeculate`).
// Lane "site": `Tgt.spin` called from the compiled `Caller.drive`, so its exit
// is resumed by the compiled caller's call-site service
// (`helpers::try_resume_trapped_callee`).
//
// Run it with OSR off, so each loop runs in a method-entry body:
//     CRATONVM_JIT_OSR=0
// Before wave 46 the method-entry body of `Tgt.spin` (the loop's own class)
// was spared by the redefinition's force pass and kept running compiled,
// translating every constant-pool index it reached into the merged pool.
// Wave 46 forces it and, once its frame is known to translate, sends it to
// the interpreter at its next exit; the sink rebuilds the frame in the
// bytecode the body was compiled from and runs the rest of the activation
// interpreted. The output is the same either way; what differs is the
// positive control:
//
//   CRATONVM_DBG_JITC=1  ->  [cratonvm-jitc] own-class method-entry body leaves (renumbered pool): ...$Tgt.spin...
//   CRATONVM_DBG_DEOPT=1 ->  [cratonvm-deopt] renumbered method-entry body ...$Tgt.spin...: its frame translates=true ...
//                            [cratonvm-deopt] withdrawn body told to leave: ...$Tgt.spin:(Ljava/lang/String;JI)I verdict=0x..
//                            then, per lane that left, one sink line:
//                            [cratonvm-deopt] ...$Tgt.spin:(Ljava/lang/String;JI)I bci=..: resuming in the bytecode the body was compiled from ...   (door)
//                            [cratonvm-deopt] ...$Tgt.spin:(Ljava/lang/String;JI)I bci=..: renumbered obsolete activation resumed by the call-site service ...   (site)
//
// None of these lines names `Tgt.spin` on the base (55834015b): its
// method-entry body is spared and never told to leave. If `Caller.drive`
// spliced `Tgt.spin`, the site lane leaves through the splice rules instead
// (a withdrawn body of ANOTHER class); `CRATONVM_DBG_JITC=1` then shows no
// method-entry leave line for that lane.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     door wrong=0 threw=none after-swap-laps=true
//     door old-activation-constant=spinA
//     site wrong=0 threw=none after-swap-laps=true
//     site old-activation-constant=spinA
//     fresh=tagB
// On a failure CratonVM also prints `threw-message=` and the `threw-at`
// frames of that lane. A redefinition race: the orchestrator repeats it 60
// times, interleaved against the base, with and without CRATONVM_JIT_OSR=0.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W46OwnClassEntryBodyLeavesAtRenumbering$Agent
//     Can-Redefine-Classes: true
// containing L2W46OwnClassEntryBodyLeavesAtRenumbering*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W46OwnClassEntryBodyLeavesAtRenumbering
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;

public class L2W46OwnClassEntryBodyLeavesAtRenumbering {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    /** The class the loop names by `ldc`, `new`, `anewarray` and `multianewarray`. */
    public static class Mark {
    }

    static final Class<?> MARK = Mark.class;
    static final Class<?> MARK_ARRAY = Mark[].class;
    static final Class<?> MARK_GRID = Mark[][].class;

    /** The class as first loaded. */
    public static class Tgt {
        static volatile boolean stop;
        static volatile long lapsDoor;
        static volatile long lapsSite;
        static volatile String lastDoor;
        static volatile String lastSite;

        public static String tag() {
            return "tagA";
        }

        public static int spin(String expect, long limit, int lane) {
            int wrong = 0;
            long n = 0;
            while (!stop && n < limit) {
                String s = "spinA";
                if (s != expect) {
                    wrong++;
                }
                Class<?> k = Mark.class;
                if (k != MARK) {
                    wrong++;
                }
                Object o = new Mark();
                if (o.getClass() != MARK) {
                    wrong++;
                }
                Object[] a = new Mark[1];
                if (a.getClass() != MARK_ARRAY) {
                    wrong++;
                }
                Object[][] g = new Mark[1][1];
                if (g.getClass() != MARK_GRID) {
                    wrong++;
                }
                if (lane == 0) {
                    lastDoor = s;
                    lapsDoor++;
                } else {
                    lastSite = s;
                    lapsSite++;
                }
                n++;
            }
            return wrong;
        }
    }

    /** The edited source, as javac compiled it: renamed to Tgt before use. */
    public static class Dgt {
        static volatile boolean stop;
        static volatile long lapsDoor;
        static volatile long lapsSite;
        static volatile String lastDoor;
        static volatile String lastSite;

        public static String tag() {
            String pad = "padB";
            StringBuilder b = new StringBuilder("wideB");
            if (pad.length() == b.length() + 7) {
                return "twoB";
            }
            return "tagB";
        }

        public static int spin(String expect, long limit, int lane) {
            int wrong = 0;
            long n = 0;
            while (!stop && n < limit) {
                String s = "spinB";
                if (s != expect) {
                    wrong++;
                }
                Class<?> k = Mark.class;
                if (k != MARK) {
                    wrong++;
                }
                Object o = new Mark();
                if (o.getClass() != MARK) {
                    wrong++;
                }
                Object[] a = new Mark[1];
                if (a.getClass() != MARK_ARRAY) {
                    wrong++;
                }
                Object[][] g = new Mark[1][1];
                if (g.getClass() != MARK_GRID) {
                    wrong++;
                }
                if (lane == 0) {
                    lastDoor = s;
                    lapsDoor++;
                } else {
                    lastSite = s;
                    lapsSite++;
                }
                n++;
            }
            return wrong;
        }
    }

    /** A compiled caller of `Tgt.spin`: the site lane's call-site service. */
    public static class Caller {
        public static int drive(String expect, long limit, int calls) {
            int wrong = 0;
            for (int c = 0; c < calls; c++) {
                wrong += Tgt.spin(expect, limit, 1);
            }
            return wrong;
        }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W46OwnClassEntryBodyLeavesAtRenumbering.class
                .getResourceAsStream("L2W46OwnClassEntryBodyLeavesAtRenumbering$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `donor`'s class file with every occurrence of its name turned into `target`'s. */
    static byte[] renamed(byte[] b, String donor, String target) {
        String from = "L2W46OwnClassEntryBodyLeavesAtRenumbering$" + donor;
        String to = "L2W46OwnClassEntryBodyLeavesAtRenumbering$" + target;
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

    static volatile int resultDoor = -1;
    static volatile int resultSite = -1;
    static volatile Throwable thrownDoor;
    static volatile Throwable thrownSite;
    static volatile boolean warmDoor;
    static volatile boolean warmSite;

    /** Enough short calls for a method-entry compile of `spin` (and `drive`). */
    static final int WARM_CALLS = 20_000;
    static final long WARM_LIMIT = 50;

    static void waitFor(java.util.function.BooleanSupplier done) throws InterruptedException {
        long deadline = System.nanoTime() + 60_000_000_000L;
        while (!done.getAsBoolean() && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
    }

    static void report(String lane, int result, Throwable t, boolean moved, String seen, Thread worker) {
        System.out.println(lane + " wrong=" + result + " threw="
                + (t == null ? "none" : t.getClass().getName())
                + " after-swap-laps=" + moved
                + (worker.isAlive() ? " (still running)" : ""));
        System.out.println(lane + " old-activation-constant=" + seen);
        if (t != null) {
            System.out.println(lane + " threw-message=" + t.getMessage());
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println(lane + " threw-at " + e);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] edited = renamed(bytesOf("Dgt"), "Dgt", "Tgt");
        Tgt.tag();
        Thread door = new Thread(() -> {
            try {
                for (int c = 0; c < WARM_CALLS; c++) {
                    Tgt.spin("spinA", WARM_LIMIT, 0);
                }
                warmDoor = true;
                resultDoor = Tgt.spin("spinA", Long.MAX_VALUE, 0);
            } catch (Throwable t) {
                thrownDoor = t;
            }
        }, "door-spinner");
        Thread site = new Thread(() -> {
            try {
                for (int c = 0; c < WARM_CALLS; c++) {
                    Caller.drive("spinA", WARM_LIMIT, 1);
                }
                warmSite = true;
                resultSite = Caller.drive("spinA", Long.MAX_VALUE, 1);
            } catch (Throwable t) {
                thrownSite = t;
            }
        }, "site-spinner");
        door.setDaemon(true);
        site.setDaemon(true);
        door.start();
        site.start();
        waitFor(() -> warmDoor && warmSite);
        long doorWarm = Tgt.lapsDoor;
        long siteWarm = Tgt.lapsSite;
        // Both long activations well under way before the swap.
        waitFor(() -> Tgt.lapsDoor > doorWarm + 200_000 && Tgt.lapsSite > siteWarm + 200_000);
        Thread.sleep(50);
        i.redefineClasses(new ClassDefinition(Tgt.class, edited));
        long doorAtSwap = Tgt.lapsDoor;
        long siteAtSwap = Tgt.lapsSite;
        // The old activations must keep looping, on their own constants.
        waitFor(() -> Tgt.lapsDoor > doorAtSwap + 50_000 && Tgt.lapsSite > siteAtSwap + 50_000);
        boolean doorMoved = Tgt.lapsDoor > doorAtSwap;
        boolean siteMoved = Tgt.lapsSite > siteAtSwap;
        String doorSeen = Tgt.lastDoor;
        String siteSeen = Tgt.lastSite;
        Tgt.stop = true;
        door.join(60_000);
        site.join(60_000);
        report("door", resultDoor, thrownDoor, doorMoved, doorSeen, door);
        report("site", resultSite, thrownSite, siteMoved, siteSeen, site);
        System.out.println("fresh=" + Tgt.tag());
    }
}
