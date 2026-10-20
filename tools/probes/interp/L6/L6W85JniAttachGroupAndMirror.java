// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 85, lane L6 -- what `AttachCurrentThread` builds,
// and when. Fixed page:
// docs/internal/fixed-bugs/interpreter-i84-L6-jni-attach-group-and-lazy-mirror-FIXED-20261118.md.
//
// HotSpot 25 (jni.cpp `attach_current_thread`, javaThread.cpp
// `allocate_threadObj`) builds the attached thread's `java.lang.Thread` BEFORE
// the attach returns:
// * in `JavaVMAttachArgs.group` (the main group when null);
// * named by `JavaVMAttachArgs.name`, decoded as MODIFIED UTF-8 (a
//   supplementary character arrives as a CESU-8 surrogate pair), or
//   `"Thread-" + ThreadNumbering.next()` at that moment;
// * so a thread that attached and has not called Java yet is already listed
//   by `Thread.getAllStackTraces()` (RUNNABLE: it is in native code).
//
// Before wave 85 CratonVM ignored the group (always "main"), refused a CESU-8
// name (strict UTF-8; the thread got a default name), and built the `Thread`
// on the thread's first `Thread.currentThread()`, so the two held threads
// below were not listed at all and drew their numbers late.
//
// Build and run (Linux):
//   gcc -shared -fPIC -pthread -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6w85jni.so tools/probes/interp/L6/L6W85JniAttachGroupAndMirror.c
//   javac -d /tmp/l6w85jni tools/probes/interp/L6/L6W85JniAttachGroupAndMirror.java
//   java     -cp /tmp/l6w85jni L6W85JniAttachGroupAndMirror /tmp/libl6w85jni.so
//   cratonvm -cp /tmp/l6w85jni L6W85JniAttachGroupAndMirror /tmp/libl6w85jni.so
//   cratonvm --compatible -cp /tmp/l6w85jni L6W85JniAttachGroupAndMirror /tmp/libl6w85jni.so
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// new Thread() before: Thread-0
// attach in group: name=Thread-1 group=custom parent=main priority=5 daemon=false
// attach named (CESU-8): codepoints=1d11e,78 group=main
// held attached threads listed before calling Java: [Thread-2, Thread-3]
// their states: [RUNNABLE, RUNNABLE]
// after release: alive=[false, false]
// new Thread() after: Thread-4
// HOTSPOT_EXPECTED_END

import java.util.ArrayList;
import java.util.Collections;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L6W85JniAttachGroupAndMirror {
    static native void attachInGroup(ThreadGroup g);
    static native void attachNamed(byte[] modifiedUtf8);
    static native void attachAndHold(int n);
    static native int attachedCount();
    static native void release();

    static volatile CountDownLatch latch;
    static volatile String info;

    static void attached() {
        Thread t = Thread.currentThread();
        ThreadGroup g = t.getThreadGroup();
        StringBuilder cps = new StringBuilder();
        t.getName().codePoints().forEach(cp -> {
            if (cps.length() > 0) {
                cps.append(',');
            }
            cps.append(Integer.toHexString(cp));
        });
        info = "name=" + t.getName()
                + " codepoints=" + cps
                + " group=" + (g == null ? "null" : g.getName())
                + " parent=" + (g == null || g.getParent() == null ? "null" : g.getParent().getName())
                + " priority=" + t.getPriority()
                + " daemon=" + t.isDaemon();
        latch.countDown();
    }

    static String field(String all, String key) {
        for (String part : all.split(" ")) {
            if (part.startsWith(key + "=")) {
                return part;
            }
        }
        return key + "=?";
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        System.out.println("new Thread() before: " + new Thread().getName());

        ThreadGroup custom = new ThreadGroup("custom");
        latch = new CountDownLatch(1);
        attachInGroup(custom);
        System.out.println("attach in group: "
                + (latch.await(10, TimeUnit.SECONDS)
                    ? field(info, "name") + " " + field(info, "group") + " " + field(info, "parent")
                        + " " + field(info, "priority") + " " + field(info, "daemon")
                    : "never called"));

        // U+1D11E then 'x', as modified UTF-8: a CESU-8 surrogate pair.
        byte[] cesu = {(byte) 0xED, (byte) 0xA0, (byte) 0xB4, (byte) 0xED, (byte) 0xB4, (byte) 0x9E, 'x'};
        latch = new CountDownLatch(1);
        attachNamed(cesu);
        System.out.println("attach named (CESU-8): "
                + (latch.await(10, TimeUnit.SECONDS)
                    ? field(info, "codepoints") + " " + field(info, "group")
                    : "never called"));
        Thread.sleep(200); // let the two detaches above finish

        Set<Thread> before = new HashSet<>(Thread.getAllStackTraces().keySet());
        attachAndHold(2);
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (attachedCount() < 2 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        List<Thread> held = new ArrayList<>();
        for (Thread t : Thread.getAllStackTraces().keySet()) {
            if (!before.contains(t)) {
                held.add(t);
            }
        }
        held.sort((x, y) -> x.getName().compareTo(y.getName()));
        List<String> names = new ArrayList<>();
        List<String> states = new ArrayList<>();
        for (Thread t : held) {
            names.add(t.getName());
            states.add(String.valueOf(t.getState()));
        }
        System.out.println("held attached threads listed before calling Java: " + names);
        System.out.println("their states: " + states);
        release();
        List<Boolean> alive = new ArrayList<>();
        for (Thread t : held) {
            t.join(TimeUnit.SECONDS.toMillis(10));
            alive.add(t.isAlive());
        }
        System.out.println("after release: alive=" + alive);
        System.out.println("new Thread() after: " + new Thread().getName());
    }
}
