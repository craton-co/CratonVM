// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L6 -- JNI-attached threads: their
// `Thread-N` name, daemon status, group and priority, the attach-args version
// rule, and what `DetachCurrentThread` does to their `Thread` object.
//
// HotSpot 25 (jni.cpp `attach_current_thread`, javaThread.cpp
// `allocate_threadObj`, `jni_DetachCurrentThread`):
// * `JavaVMAttachArgs` is read only when `Threads::is_supported_jni_version
//   (args->version)` -- 1.2 and later; 1.1 is not -- else the attach is as if
//   no args were given (no error);
// * no name: `Thread(ThreadGroup, Runnable)`, so the name is
//   `"Thread-" + ThreadNumbering.next()`, the counter every unnamed
//   `new Thread()` draws from; an EMPTY name is the name "";
// * the group is the main thread group, the priority NORM_PRIORITY, daemon
//   per `AttachCurrentThreadAsDaemon`;
// * detach runs `Thread.exit()` and `ensure_join`: the `Thread` is
//   TERMINATED, `isAlive()` false, and `join()` returns.
//
// Before wave 84 CratonVM named an unnamed attached thread `Thread-<VM
// registry id>` (a number from another sequence), named an empty-named one
// that way too, read the args whatever their version, and left a detached
// thread's `Thread` alive: `join()` never returned (this probe waits 10 s).
//
// Build and run (Linux):
//   gcc -shared -fPIC -pthread -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6w84jni.so tools/probes/interp/L6/L6W84JniAttachedThreads.c
//   javac -d /tmp/l6w84jni tools/probes/interp/L6/L6W84JniAttachedThreads.java
//   java     -cp /tmp/l6w84jni L6W84JniAttachedThreads /tmp/libl6w84jni.so
//   cratonvm -cp /tmp/l6w84jni L6W84JniAttachedThreads /tmp/libl6w84jni.so
//   cratonvm --compatible -cp /tmp/l6w84jni L6W84JniAttachedThreads /tmp/libl6w84jni.so
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// new Thread() before: Thread-0
// attach(NULL): name=Thread-1 daemon=false group=main priority=5 alive=true
//   after detach: joined=true alive=false state=TERMINATED
// attachAsDaemon(NULL): name=Thread-2 daemon=true group=main priority=5 alive=true
//   after detach: joined=true alive=false state=TERMINATED
// attach(1.2, "named"): name=named daemon=false group=main priority=5 alive=true
//   after detach: joined=true alive=false state=TERMINATED
// attach(1.2, ""): name= daemon=false group=main priority=5 alive=true
//   after detach: joined=true alive=false state=TERMINATED
// attach(1.1, "ignored"): name=Thread-3 daemon=false group=main priority=5 alive=true
//   after detach: joined=true alive=false state=TERMINATED
// new Thread() after: Thread-4
// HOTSPOT_EXPECTED_END

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L6W84JniAttachedThreads {
    static native void runAttached(int mode);

    static volatile CountDownLatch latch;
    static volatile Thread last;
    static volatile String info;

    static void attached() {
        Thread t = Thread.currentThread();
        ThreadGroup g = t.getThreadGroup();
        info = "name=" + t.getName()
                + " daemon=" + t.isDaemon()
                + " group=" + (g == null ? "null" : g.getName())
                + " priority=" + t.getPriority()
                + " alive=" + t.isAlive();
        last = t;
        latch.countDown();
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        System.out.println("new Thread() before: " + new Thread().getName());
        String[] labels = {
            "attach(NULL)",
            "attachAsDaemon(NULL)",
            "attach(1.2, \"named\")",
            "attach(1.2, \"\")",
            "attach(1.1, \"ignored\")",
        };
        for (int mode = 0; mode < labels.length; mode++) {
            latch = new CountDownLatch(1);
            last = null;
            info = "never attached";
            runAttached(mode);
            boolean called = latch.await(10, TimeUnit.SECONDS);
            System.out.println(labels[mode] + ": " + (called ? info : "never called"));
            Thread t = last;
            if (t == null) {
                continue;
            }
            t.join(TimeUnit.SECONDS.toMillis(10));
            System.out.println("  after detach: joined=" + !t.isAlive()
                    + " alive=" + t.isAlive() + " state=" + t.getState());
        }
        System.out.println("new Thread() after: " + new Thread().getName());
    }
}
