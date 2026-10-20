// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L6 -- what Thread answers about itself,
// platform and virtual, against HotSpot 25: names (Thread-N numbering,
// ForkJoinPool-N-worker-M, the empty virtual name), toString (thread ids
// shown as #N except main's), getThreadGroup (and null once terminated),
// daemon status and its inheritance, setDaemon(false) on a virtual thread,
// priorities, Thread.ofVirtual().unstarted answers (state, isAlive, join),
// ThreadLocal / InheritableThreadLocal on virtual threads (and
// inheritInheritableThreadLocals(false)), and the default uncaught-exception
// output of a platform and a virtual thread (System.err captured, printed to
// stdout). No timing in the output.
import java.io.ByteArrayOutputStream;
import java.io.PrintStream;
import java.time.Duration;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.atomic.AtomicReference;

public class L6W98ThreadIdentity {
    static String norm(String s) {
        return s.replaceAll("#\\d+", "#N").replaceAll("worker-\\d+", "worker-M");
    }

    static final ThreadLocal<String> TL = new ThreadLocal<>();
    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>();

    static class Boom implements Runnable {
        public void run() {
            throw new IllegalStateException("boom");
        }
    }

    public static void main(String[] args) throws Exception {
        Thread main = Thread.currentThread();
        System.out.println("main " + main + " id=" + main.threadId() + " daemon=" + main.isDaemon()
                + " prio=" + main.getPriority());
        ThreadGroup g = main.getThreadGroup();
        System.out.println("main group=" + g.getName() + " parent=" + g.getParent().getName()
                + " grandparent=" + g.getParent().getParent() + " maxPrio=" + g.getMaxPriority()
                + " daemon=" + g.isDaemon());

        // Thread-N numbering: only unnamed platform threads take a number.
        Thread a = new Thread(() -> { });
        Thread named = new Thread(() -> { }, "named");
        Thread b = new Thread(() -> { });
        Thread built = Thread.ofPlatform().unstarted(() -> { });
        Thread c = new Thread(() -> { });
        System.out.println("names " + a.getName() + " " + named.getName() + " " + b.getName() + " "
                + built.getName() + " " + c.getName());
        Thread counted = Thread.ofPlatform().name("w-", 7).unstarted(() -> { });
        Thread counted2 = Thread.ofPlatform().name("w-", 7).unstarted(() -> { });
        System.out.println("builder names " + counted.getName() + " " + counted2.getName());
        System.out.println("unstarted toString " + norm(a.toString()) + " state=" + a.getState());
        a.start();
        a.join();
        System.out.println("terminated toString " + norm(a.toString()) + " group=" + a.getThreadGroup()
                + " alive=" + a.isAlive());

        // daemon inheritance
        AtomicReference<String> seen = new AtomicReference<>();
        Thread d = new Thread(() -> {
            Thread inner = new Thread(() -> { });
            seen.set("inner-daemon=" + inner.isDaemon() + " inner-prio=" + inner.getPriority());
        });
        d.setDaemon(true);
        d.setPriority(7);
        d.start();
        d.join();
        System.out.println("daemon parent -> " + seen.get());
        System.out.println("setDaemon on started -> " + tryIt(() -> {
            Thread t = new Thread(() -> {
                try {
                    Thread.sleep(200);
                } catch (InterruptedException e) {
                    // ignore
                }
            });
            t.start();
            try {
                t.setDaemon(true);
            } finally {
                t.interrupt();
                t.join();
            }
        }));

        // virtual threads
        Thread vu = Thread.ofVirtual().unstarted(() -> { });
        System.out.println("vthread unstarted name='" + vu.getName() + "' state=" + vu.getState() + " alive="
                + vu.isAlive() + " virtual=" + vu.isVirtual() + " daemon=" + vu.isDaemon() + " prio="
                + vu.getPriority() + " toString=" + norm(vu.toString()));
        System.out.println("vthread unstarted join(Duration) -> " + vu.join(Duration.ofMillis(1)));
        System.out.println("vthread unstarted join() -> " + tryIt(vu::join));
        System.out.println("vthread setDaemon(false) -> " + tryIt(() -> vu.setDaemon(false)));
        System.out.println("vthread setDaemon(true) -> " + tryIt(() -> vu.setDaemon(true)));
        vu.setPriority(1);
        System.out.println("vthread setPriority(1) -> prio=" + vu.getPriority());
        ThreadGroup vg = vu.getThreadGroup();
        System.out.println("vthread group=" + vg.getName() + " parent=" + vg.getParent().getName() + " maxPrio="
                + vg.getMaxPriority());
        Thread vn = Thread.ofVirtual().name("vee").unstarted(() -> { });
        System.out.println("named vthread toString=" + norm(vn.toString()));
        AtomicReference<String> inside = new AtomicReference<>();
        Thread vr = Thread.ofVirtual().start(() -> {
            Thread me = Thread.currentThread();
            inside.set(norm(me.toString()) + " group=" + me.getThreadGroup().getName());
        });
        vr.join();
        System.out.println("running vthread " + inside.get().replaceAll("/runnable@ForkJoinPool-\\d+", "/runnable@ForkJoinPool-P"));
        System.out.println("terminated vthread toString=" + norm(vr.toString()) + " group=" + vr.getThreadGroup()
                + " join(Duration)=" + vr.join(Duration.ofMillis(1)));
        Thread vb = Thread.ofVirtual().name("v-", 3).unstarted(() -> { });
        System.out.println("vthread counted name=" + vb.getName());

        // ForkJoinPool worker names
        ForkJoinPool pool = new ForkJoinPool(1);
        String worker = pool.submit(() -> Thread.currentThread().getName()).get();
        pool.shutdown();
        System.out.println("fjp worker " + worker.replaceAll("ForkJoinPool-\\d+", "ForkJoinPool-P"));
        String common = ForkJoinPool.commonPool().submit(() -> Thread.currentThread().getName()).get();
        System.out.println("common worker " + common.replaceAll("worker-\\d+", "worker-M"));

        // thread locals on virtual threads
        TL.set("main-tl");
        ITL.set("main-itl");
        AtomicReference<String> tl = new AtomicReference<>();
        Thread v1 = Thread.ofVirtual().start(() -> {
            String before = TL.get() + "/" + ITL.get();
            TL.set("v-tl");
            ITL.set("v-itl");
            tl.set(before + " -> " + TL.get() + "/" + ITL.get());
        });
        v1.join();
        System.out.println("vthread locals " + tl.get() + " main still " + TL.get() + "/" + ITL.get());
        Thread v2 = Thread.ofVirtual().inheritInheritableThreadLocals(false)
                .start(() -> tl.set(TL.get() + "/" + ITL.get()));
        v2.join();
        System.out.println("vthread no-inherit " + tl.get());
        Thread p2 = Thread.ofPlatform().inheritInheritableThreadLocals(false)
                .start(() -> tl.set(TL.get() + "/" + ITL.get()));
        p2.join();
        System.out.println("platform no-inherit " + tl.get());

        // uncaught exception output
        PrintStream err = System.err;
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        System.setErr(new PrintStream(buf, true));
        try {
            Thread p = new Thread(new Boom(), "boomer");
            p.start();
            p.join();
            Thread v = Thread.ofVirtual().unstarted(new Boom());
            v.start();
            v.join();
            Thread vnamed = Thread.ofVirtual().name("vboom").unstarted(new Boom());
            vnamed.start();
            vnamed.join();
        } finally {
            System.err.flush();
            System.setErr(err);
        }
        System.out.print("uncaught:\n" + buf.toString().replace("\r\n", "\n"));
        System.out.println("done");
    }

    interface Body {
        void run() throws Exception;
    }

    static String tryIt(Body b) {
        try {
            b.run();
            return "ok";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }
}
