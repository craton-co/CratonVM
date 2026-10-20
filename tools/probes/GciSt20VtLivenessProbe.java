// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i20/st: the GciVd19VtChurnProbe workload (virtual threads that unmount
 * once per iteration under a deep stack and allocate after it) plus a
 * LIVENESS WATCHDOG that names a quiet hang instead of timing out on it.
 *
 *   GciSt20VtLivenessProbe [THREADS [ITERS [DEPTH [MODE [STALL_SECS [NUDGE [STACKS]]]]]]]
 *     defaults 64 200 12 mixed 10 1 1
 *   STACKS=0 leaves Thread.getAllStackTraces out of a stall record (if a stack
 *   walk itself waits on the hung VM, the ThreadGroup list and the nudges
 *   still run).
 *   MODE: mixed (Thread.yield / LockSupport.parkNanos(50us) by parity, as
 *         vd19), yield (Thread.yield only), park (parkNanos only),
 *         wait (gci i25/vt: a timed Object.wait(1 ms) on the thread's own
 *         lock, nobody notifies -- the TIMED_WAITING unmount), waitpark
 *         (timed Object.wait / parkNanos(50us) by parity).
 *
 * The workload and the checks are vd19's, value for value, so stdout is the
 * same pure function of the arguments on every VM:
 *   st20 liveness threads=64 iters=200 depth=12 mode=mixed mismatches=0 checksum=<n>
 *
 * The watchdog (a daemon platform thread, stderr only) counts finished
 * iterations. When none finishes for STALL_SECS it prints one STALL record:
 *   [st20] stall=K ...           progress, elapsed, how many threads are done
 *   [st20] vt-states ...         unfinished virtual threads by Thread.getState()
 *                                and by where the probe left them (in-park,
 *                                in-yield, running), with sample ids
 *   [st20] T name=... state=...  every platform thread (Thread.getAllStackTraces:
 *   [st20]   at ...              the carriers, the ForkJoinPool delay scheduler,
 *                                main), top frames
 * and then, with NUDGE=1, wakes one class of thread at a time and waits for
 * progress after each (spurious wake-ups are legal for every one of them):
 *   stage 1 delay-scheduler   LockSupport.unpark of "*-delayScheduler" (the
 *                             JDK 25 thread that fires a parked virtual
 *                             thread's parkNanos timeout)
 *   stage 2 carriers          every "ForkJoinPool*" platform thread
 *   stage 3 vthreads          every unfinished virtual thread
 * printing "[st20] stall=K resumed-after=<stage>" (the first stage after
 * which an iteration finished) or "[st20] stall=K none-progressed". The
 * first stage that resumes the run names the lost wake-up: the scheduler's
 * own park (stage 1), a ForkJoinPool worker signal (stage 2), or the
 * parked virtual threads' timeout tasks / unparks (stage 3). At the end:
 *   [st20] stalls=K outcomes=[...] ms=<n>
 * HotSpot 25 finishes the default arguments in about 100 ms, so it never
 * prints a stall record.
 */
public class GciSt20VtLivenessProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final AtomicLong CHECKSUM = new AtomicLong();
    static final AtomicLong PROGRESS = new AtomicLong();
    static volatile Object sink;

    static final int PH_RUNNING = 0;
    static final int PH_PARK = 1;
    static final int PH_YIELD = 2;
    static final int PH_DONE = 3;
    static final int PH_WAIT = 4;

    static int mode; // 0 mixed, 1 yield, 2 park, 3 wait, 4 waitpark
    static Object[] locks; // gci i25/vt: one per thread, for the wait modes
    static boolean stacks = true; // STACKS: Thread.getAllStackTraces in a stall record
    static AtomicIntegerArray phase;
    static AtomicIntegerArray iter;
    static final StringBuffer OUTCOMES = new StringBuffer();
    static final AtomicInteger STALLS = new AtomicInteger();

    static final class Node {
        final int v;
        final long[] pad;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
            this.pad = new long[] { v, v * 3L, v * 7L };
        }
    }

    static final class Holder {
        final int id;
        Node young;

        Holder(int id) {
            this.id = id;
        }
    }

    static void garbage(int k) {
        for (int i = 0; i < k; i++) {
            sink = new byte[64 + (i * 37 % 512)];
        }
    }

    static boolean good(Node n, int v) {
        return n != null && n.v == v && n.pad != null && n.pad.length == 3 && n.pad[0] == v
                && n.pad[1] == v * 3L && n.pad[2] == v * 7L;
    }

    static void bad(int id, int it, String what) {
        if (MISMATCH.incrementAndGet() <= 20) {
            System.err.println("[st20] mismatch id=" + id + " it=" + it + " " + what);
        }
    }

    static int base(int id, int it, int depth) {
        return id * 10000 + it * 100 + depth;
    }

    static boolean yields(int id, int it) {
        if (mode == 1) {
            return true;
        }
        if (mode == 2) {
            return false;
        }
        return ((id + it) & 1) == 0;
    }

    // gci i25/vt: the wait modes' unmount (wait: always; waitpark: by parity).
    static boolean waits(int id, int it) {
        return mode == 3 || (mode == 4 && ((id + it) & 1) == 0);
    }

    // gci i25/vt: a timed Object.wait nobody notifies; it ends by its timeout.
    static void timedWait(int id) {
        Object lock = locks[id];
        synchronized (lock) {
            try {
                lock.wait(1L);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        }
    }

    // The unmount. Its result lands on the caller's operand stack above `local`.
    static int pause(int id, int it) {
        if (waits(id, it)) {
            phase.set(id, PH_WAIT);
            timedWait(id);
        } else if (yields(id, it)) {
            phase.set(id, PH_YIELD);
            Thread.yield();
        } else {
            phase.set(id, PH_PARK);
            LockSupport.parkNanos(50_000L);
        }
        phase.set(id, PH_RUNNING);
        garbage(48);
        return it;
    }

    static long combine(Node head, int it, int id, int depth) {
        if (!good(head, base(id, it, depth))) {
            bad(id, it, "operand-stack head");
            return 0;
        }
        return head.pad[2] + it;
    }

    static long deep(Holder h, int depth, Node carry, int id, int it) {
        int b = base(id, it, depth);
        Node local = new Node(b, carry);
        Node scoped = new Node(-b, null);
        long s = scoped.pad[1]; // the last read of `scoped`
        if (depth > 0) {
            s += deep(h, depth - 1, local, id, it);
        } else {
            h.young = new Node(b + 7, local);
            s += combine(local, pause(id, it), id, depth);
            Node y = h.young;
            if (!good(y, b + 7) || y.next != local) {
                bad(id, it, "holder.young");
            } else {
                s += y.pad[1];
            }
        }
        if (!good(local, b)) {
            bad(id, it, "local depth=" + depth);
        } else if (carry != null && (local.next != carry || !good(carry, b + 1))) {
            bad(id, it, "carry depth=" + depth);
        } else {
            s += local.v + (carry == null ? 0L : carry.pad[2]);
        }
        return s;
    }

    static void run(int id, int iters, int depth) {
        Holder h = new Holder(id);
        Node old = new Node(id, null);
        long s = 0;
        for (int it = 0; it < iters; it++) {
            s += deep(h, depth, null, id, it);
            if (!good(old, id)) {
                bad(id, it, "old");
            }
            iter.set(id, it + 1);
            PROGRESS.incrementAndGet();
        }
        CHECKSUM.addAndGet(s + old.pad[1] + h.id);
    }

    static void body(int id, int iters, int depth) {
        try {
            run(id, iters, depth);
        } catch (Throwable t) {
            bad(id, -1, "threw " + t.getClass().getName());
        } finally {
            phase.set(id, PH_DONE);
        }
    }

    /** gci i24/vl: VirtualThread.state of every unfinished thread, by value (empty when not readable). */
    static TreeMap<Integer, Integer> internalStates(List<Thread> ts) {
        TreeMap<Integer, Integer> out = new TreeMap<>();
        java.lang.reflect.Field f;
        try {
            f = Class.forName("java.lang.VirtualThread").getDeclaredField("state");
            f.setAccessible(true);
        } catch (Throwable t) {
            return out;
        }
        for (int i = 0; i < ts.size(); i++) {
            if (phase.get(i) == PH_DONE) {
                continue;
            }
            try {
                out.merge(f.getInt(ts.get(i)), 1, Integer::sum);
            } catch (Throwable t) {
                return out;
            }
        }
        return out;
    }

    static String phaseName(int p) {
        switch (p) {
            case PH_RUNNING:
                return "running";
            case PH_PARK:
                return "in-park";
            case PH_YIELD:
                return "in-yield";
            case PH_WAIT:
                return "in-wait";
            default:
                return "done";
        }
    }

    /** One STALL record: progress, the unfinished virtual threads, the platform threads' stacks. */
    static void report(int stall, List<Thread> ts, long total, long t0, long stalledMs) {
        int n = ts.size();
        int done = 0;
        TreeMap<String, Integer> hist = new TreeMap<>();
        TreeMap<String, StringBuilder> samples = new TreeMap<>();
        for (int i = 0; i < n; i++) {
            int p = phase.get(i);
            if (p == PH_DONE) {
                done++;
                continue;
            }
            Thread t = ts.get(i);
            String key = t.getState() + "/" + phaseName(p);
            hist.merge(key, 1, Integer::sum);
            StringBuilder sb = samples.computeIfAbsent(key, k -> new StringBuilder());
            if (sb.length() < 120) {
                sb.append(' ').append(i).append('@').append(iter.get(i));
            }
        }
        System.err.println("[st20] stall=" + stall + " elapsed_ms=" + ((System.nanoTime() - t0) / 1_000_000)
                + " stalled_ms=" + stalledMs + " progress=" + PROGRESS.get() + "/" + total
                + " done=" + done + "/" + n + " mismatches=" + MISMATCH.get());
        for (Map.Entry<String, Integer> e : hist.entrySet()) {
            System.err.println("[st20] vt-states " + e.getKey() + "=" + e.getValue() + " ids(id@iter):"
                    + samples.get(e.getKey()));
        }
        // gci i24/vl: the JDK's own state of the unfinished virtual threads
        // (VirtualThread.state, JDK 25 numbering: 6 TIMED_PARKING, 7
        // TIMED_PARKED, 9 UNPARKED, 2 RUNNING; gci i25/vt's wait modes: 17
        // TIMED_WAITING, 18 TIMED_WAIT, 13 BLOCKED, 14 UNBLOCKED). Needs
        // --add-opens java.base/java.lang=ALL-UNNAMED; skipped without it.
        TreeMap<Integer, Integer> internal = internalStates(ts);
        if (!internal.isEmpty()) {
            System.err.println("[st20] vt-internal-states " + internal);
        }
        // The platform threads by ThreadGroup first (no stack walk, so it cannot
        // wait for a handshake), then their stacks unless STACKS=0.
        TreeMap<String, String> plain = new TreeMap<>();
        for (Thread t : platformThreads()) {
            plain.put(t.getName() + "#" + t.threadId(), "[st20] P name=" + t.getName() + " state=" + t.getState()
                    + " daemon=" + t.isDaemon());
        }
        for (String s : plain.values()) {
            System.err.println(s);
        }
        if (!stacks) {
            return;
        }
        Map<Thread, StackTraceElement[]> all = Thread.getAllStackTraces();
        TreeMap<String, String> lines = new TreeMap<>();
        for (Map.Entry<Thread, StackTraceElement[]> e : all.entrySet()) {
            Thread t = e.getKey();
            if (t == Thread.currentThread()) {
                continue;
            }
            StringBuilder sb = new StringBuilder();
            sb.append("[st20] T name=").append(t.getName()).append(" state=").append(t.getState())
                    .append(" daemon=").append(t.isDaemon()).append(" virtual=").append(t.isVirtual());
            StackTraceElement[] st = e.getValue();
            for (int i = 0; i < st.length && i < 10; i++) {
                sb.append('\n').append("[st20]   at ").append(st[i]);
            }
            lines.put(t.getName() + "#" + t.threadId(), sb.toString());
        }
        for (String s : lines.values()) {
            System.err.println(s);
        }
    }

    /** Every live platform thread, from the root ThreadGroup (no stack walk). */
    static List<Thread> platformThreads() {
        ThreadGroup g = Thread.currentThread().getThreadGroup();
        while (g.getParent() != null) {
            g = g.getParent();
        }
        Thread[] arr = new Thread[g.activeCount() + 64];
        int c = g.enumerate(arr, true);
        List<Thread> out = new ArrayList<>(c);
        for (int i = 0; i < c; i++) {
            if (arr[i] != null && arr[i] != Thread.currentThread()) {
                out.add(arr[i]);
            }
        }
        return out;
    }

    /** Wake one class of thread; how many were woken. */
    static int nudge(int stage, List<Thread> ts) {
        int woken = 0;
        if (stage == 3) {
            for (int i = 0; i < ts.size(); i++) {
                if (phase.get(i) != PH_DONE) {
                    LockSupport.unpark(ts.get(i));
                    woken++;
                }
            }
            return woken;
        }
        for (Thread t : platformThreads()) {
            String name = t.getName();
            boolean pick = stage == 1 ? name.endsWith("-delayScheduler") : name.startsWith("ForkJoinPool");
            if (pick && !t.isVirtual()) {
                LockSupport.unpark(t);
                woken++;
            }
        }
        return woken;
    }

    static String stageName(int stage) {
        switch (stage) {
            case 1:
                return "delay-scheduler";
            case 2:
                return "carriers";
            default:
                return "vthreads";
        }
    }

    static void watchdog(List<Thread> ts, long total, long t0, long stallMs, boolean nudgeOn) {
        long last = PROGRESS.get();
        long lastChange = System.nanoTime();
        long stageAt = 0L;
        long stageWaitMs = Math.max(2000L, stallMs / 2);
        int stage = 0; // 0: watching; 1..3: that nudge done; 4: gave up
        int stall = 0;
        while (true) {
            try {
                Thread.sleep(250L);
            } catch (InterruptedException e) {
                return;
            }
            try {
                long now = System.nanoTime();
                long p = PROGRESS.get();
                if (p != last) {
                    if (stage >= 1 && stage <= 3) {
                        String o = "stall=" + stall + " resumed-after=" + stageName(stage);
                        System.err.println("[st20] " + o);
                        OUTCOMES.append(OUTCOMES.length() == 0 ? "" : ",").append(stageName(stage));
                    } else if (stage == 4) {
                        System.err.println("[st20] stall=" + stall + " resumed-by-itself-after-nudges");
                        OUTCOMES.append(OUTCOMES.length() == 0 ? "" : ",").append("late");
                    }
                    last = p;
                    lastChange = now;
                    stage = 0;
                    continue;
                }
                long stalledMs = (now - lastChange) / 1_000_000;
                if (stage == 0) {
                    if (stalledMs < stallMs) {
                        continue;
                    }
                    stall = STALLS.incrementAndGet();
                    report(stall, ts, total, t0, stalledMs);
                    if (!nudgeOn) {
                        stage = 4;
                        stageAt = now;
                        continue;
                    }
                    stage = 1;
                    stageAt = now;
                    System.err.println("[st20] stall=" + stall + " nudge=" + stageName(1) + " woken=" + nudge(1, ts));
                    continue;
                }
                if ((now - stageAt) / 1_000_000 < (stage == 4 ? stallMs : stageWaitMs)) {
                    continue;
                }
                if (stage < 3) {
                    stage++;
                    stageAt = now;
                    System.err.println("[st20] stall=" + stall + " nudge=" + stageName(stage) + " woken="
                            + nudge(stage, ts));
                } else {
                    if (stage == 3) {
                        System.err.println("[st20] stall=" + stall + " none-progressed");
                        OUTCOMES.append(OUTCOMES.length() == 0 ? "" : ",").append("none");
                    }
                    stage = 4;
                    stageAt = now;
                    report(stall, ts, total, t0, stalledMs);
                }
            } catch (Throwable t) {
                System.err.println("[st20] watchdog error " + t);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int depth = args.length > 2 ? Integer.parseInt(args[2]) : 12;
        String modeName = args.length > 3 ? args[3] : "mixed";
        long stallMs = 1000L * (args.length > 4 ? Integer.parseInt(args[4]) : 10);
        boolean nudgeOn = args.length > 5 ? Integer.parseInt(args[5]) != 0 : true;
        stacks = args.length > 6 ? Integer.parseInt(args[6]) != 0 : true;
        switch (modeName) {
            case "mixed":
                mode = 0;
                break;
            case "yield":
                mode = 1;
                break;
            case "park":
                mode = 2;
                break;
            case "wait":
                mode = 3;
                break;
            case "waitpark":
                mode = 4;
                break;
            default:
                throw new IllegalArgumentException("mode: mixed|yield|park|wait|waitpark");
        }
        locks = new Object[n];
        for (int i = 0; i < n; i++) {
            locks[i] = new Object();
        }
        phase = new AtomicIntegerArray(n);
        iter = new AtomicIntegerArray(n);
        long t0 = System.nanoTime();
        List<Thread> ts = new ArrayList<>(n);
        for (int i = 0; i < n; i++) {
            final int id = i;
            ts.add(Thread.ofVirtual().name("st20-" + i).unstarted(() -> body(id, iters, depth)));
        }
        final long total = (long) n * iters;
        Thread wd = new Thread(() -> watchdog(ts, total, t0, stallMs, nudgeOn), "st20-watchdog");
        wd.setDaemon(true);
        wd.start();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        System.out.println("st20 liveness threads=" + n + " iters=" + iters + " depth=" + depth + " mode="
                + modeName + " mismatches=" + MISMATCH.get() + " checksum=" + CHECKSUM.get());
        System.err.println("[st20] stalls=" + STALLS.get() + " outcomes=[" + OUTCOMES + "] ms="
                + ((System.nanoTime() - t0) / 1_000_000));
    }
}
