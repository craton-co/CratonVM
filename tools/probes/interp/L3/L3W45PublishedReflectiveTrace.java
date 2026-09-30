// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L3 -- ANOTHER thread's view of a thread
// inside a reflective call (`Method.invoke`, `Constructor.newInstance`):
// `Thread.getStackTrace()`, `Thread.getAllStackTraces()` and
// `ThreadMXBean.getThreadInfo(id, depth)` of a worker parked (or spinning)
// in a method reached reflectively. HotSpot lists the call's JDK frames
// between the target and its caller there too (item 3 of
// docs/known-issues/interpreter/i43-L3-a-throwable-a-reflective-native-raises-lists-no-reflection-frames-20261007.md;
// the proposal
// docs/internal/fixed-bugs/interpreter-L3-proposal-published-traces-carry-the-reflective-calls-FIXED-20261009.md).
//
// Rows print frames as `Class.method`, the probe's class shortened to `P`,
// from the target down to `P.workerBody` (the worker's reflective caller).
// `getAllStackTraces` and `ThreadInfo` on HotSpot also list the HIDDEN frames
// between the accessor and the target (`LambdaForm$DMH/0x....invokeStatic`,
// `LambdaForm$MH/0x....invoke`, `Invokers$Holder.invokeExact_MT`,
// `DirectMethodHandleAccessor.invokeImpl`; `Thread.getStackTrace` does not);
// their names carry addresses, and CratonVM runs no such frames, so those two
// rows leave out `java.lang.invoke.*` frames and `invokeImpl`
// (`visibleNames`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// parked-getStackTrace: P.parkTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// parked-getAllStackTraces: P.parkTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// parked-threadInfo: P.parkTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// parked-after-a-throwable: P.parkTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// parked-nested: P.parkTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.nestedTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// parked-in-constructor: P$Parker.<init>,jdk.internal.reflect.DirectConstructorHandleAccessor.newInstance,java.lang.reflect.Constructor.newInstanceWithCaller,java.lang.reflect.Constructor.newInstance,P.workerBody
// spinning-getStackTrace: P.spinTarget,jdk.internal.reflect.DirectMethodHandleAccessor.invoke,java.lang.reflect.Method.invoke,P.workerBody
// HOTSPOT_EXPECTED_END
//
// CratonVM on the base 69568bea6 (read from the code:
// `stackwalker::capture_published_trace` takes no reflective record) lists
// the target right on its caller in every row, e.g.
//     parked-getStackTrace: P.parkTarget,P.workerBody
// Since wave 45 the published trace lists the frames the thread's own
// capture lists (named when the native enters the call,
// `stackwalker::name_reflective_frames_if_stale`), in every row.
//
// Positive control: `CRATONVM_DBG_STTRACE=1` prints
// `[sttrace] publish-reflect: calls=1 listed=1 entries=<n>` when the parked
// worker publishes (`calls=2 listed=2` for `parked-nested`); the base prints
// no `publish-reflect` line.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W45PublishedReflectiveTrace`.
// A thread probe: the orchestrator repeats it (60 interleaved runs).
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.locks.LockSupport;

public class L3W45PublishedReflectiveTrace {
    static volatile boolean release;
    static volatile boolean spinning;
    static volatile Object sink;

    static String shorten(String className) {
        return className.replace("L3W45PublishedReflectiveTrace", "P");
    }

    /** Frames from the first `from`-named one down to `P.workerBody`. */
    static String names(StackTraceElement[] trace, String from) {
        List<String> out = new ArrayList<>();
        boolean on = false;
        for (StackTraceElement e : trace) {
            String n = shorten(e.getClassName()) + "." + e.getMethodName();
            if (!on && n.equals(from)) {
                on = true;
            }
            if (on) {
                out.add(n);
                if (n.equals("P.workerBody")) {
                    break;
                }
            }
        }
        return on ? String.join(",", out) : "missing " + from;
    }

    public static void parkTarget(boolean throwFirst) {
        if (throwFirst) {
            sink = new Throwable();
        }
        while (!release) {
            LockSupport.park();
        }
    }

    public static void nestedTarget() throws Exception {
        PARK.invoke(null, false);
    }

    public static void spinTarget() {
        spinning = true;
        long n = 0;
        while (!release) {
            n++;
        }
        sink = n;
    }

    public static final class Parker {
        public Parker() {
            while (!release) {
                LockSupport.park();
            }
        }
    }

    static Method PARK;

    static void workerBody(Method m, Constructor<?> c, Object... args) {
        try {
            if (m != null) {
                m.invoke(null, args);
            } else {
                c.newInstance(args);
            }
        } catch (Exception e) {
            sink = e;
        }
    }

    /** [names] without the frames HotSpot hides from `Thread.getStackTrace`. */
    static String visibleNames(StackTraceElement[] trace, String from) {
        List<StackTraceElement> kept = new ArrayList<>();
        for (StackTraceElement e : trace) {
            if (e.getClassName().startsWith("java.lang.invoke.")
                    || e.getMethodName().equals("invokeImpl")) {
                continue;
            }
            kept.add(e);
        }
        return names(kept.toArray(new StackTraceElement[0]), from);
    }

    static boolean parkedIn(Thread t, String from) {
        return t.getState() == Thread.State.WAITING
                && !names(t.getStackTrace(), from).startsWith("missing");
    }

    static Thread start(Method m, Constructor<?> c, Object... args) {
        release = false;
        spinning = false;
        Thread t = new Thread(() -> workerBody(m, c, args), "worker");
        t.setDaemon(true);
        t.start();
        return t;
    }

    static void stop(Thread t) throws InterruptedException {
        release = true;
        LockSupport.unpark(t);
        t.join(10_000);
    }

    static void waitParked(Thread t, String from) throws InterruptedException {
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (!parkedIn(t, from) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
    }

    public static void main(String[] args) throws Exception {
        PARK = L3W45PublishedReflectiveTrace.class.getMethod("parkTarget", boolean.class);
        Method nested = L3W45PublishedReflectiveTrace.class.getMethod("nestedTarget");
        Method spin = L3W45PublishedReflectiveTrace.class.getMethod("spinTarget");
        Constructor<Parker> parker = Parker.class.getConstructor();

        Thread t = start(PARK, null, false);
        waitParked(t, "P.parkTarget");
        System.out.println("parked-getStackTrace: " + names(t.getStackTrace(), "P.parkTarget"));
        StackTraceElement[] all = Thread.getAllStackTraces().get(t);
        System.out.println("parked-getAllStackTraces: "
                + (all == null ? "no entry" : visibleNames(all, "P.parkTarget")));
        ThreadInfo info = ManagementFactory.getThreadMXBean().getThreadInfo(t.threadId(), 64);
        System.out.println("parked-threadInfo: "
                + (info == null ? "no info" : visibleNames(info.getStackTrace(), "P.parkTarget")));
        stop(t);

        t = start(PARK, null, true);
        waitParked(t, "P.parkTarget");
        System.out.println("parked-after-a-throwable: " + names(t.getStackTrace(), "P.parkTarget"));
        stop(t);

        t = start(nested, null);
        waitParked(t, "P.parkTarget");
        System.out.println("parked-nested: " + names(t.getStackTrace(), "P.parkTarget"));
        stop(t);

        t = start(null, parker);
        waitParked(t, "P$Parker.<init>");
        System.out.println("parked-in-constructor: " + names(t.getStackTrace(), "P$Parker.<init>"));
        stop(t);

        t = start(spin, null);
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (!spinning && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        String seen = "missing P.spinTarget";
        while (seen.startsWith("missing") && System.nanoTime() < deadline) {
            seen = names(t.getStackTrace(), "P.spinTarget");
        }
        System.out.println("spinning-getStackTrace: " + seen);
        stop(t);
    }
}
