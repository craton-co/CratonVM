// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- what JNI `MonitorEnter` /
// `MonitorExit` leave pending (`VmConfig::jni_monitor_exceptions`,
// `CRATONVM_JNI_MONITOR_EXCEPTIONS`, default on, `=0` kill switch).
//
// HotSpot 25 (jni.cpp `jni_MonitorEnter` / `jni_MonitorExit`,
// synchronizer.cpp `ObjectSynchronizer::jni_exit`, objectMonitor.cpp
// `check_owner`): a null object is `JNI_ERR` with a `NullPointerException`
// (no message) pending; an exit of a monitor the caller does not own --
// never entered, or held by another thread -- is `JNI_ERR` with
// `IllegalMonitorStateException: current thread is not owner` pending, and
// the other thread keeps its monitor. CratonVM answered `JNI_OK` with nothing
// pending for the exit, and `JNI_ERR` with nothing pending for a null.
//
// Build and run (Linux):
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl6w101jni.so tools/probes/interp/L6/L6W101JniMonitorExitNotOwner.c
//   javac -d /tmp/l6w101jni tools/probes/interp/L6/L6W101JniMonitorExitNotOwner.java
//   java     -cp /tmp/l6w101jni L6W101JniMonitorExitNotOwner /tmp/libl6w101jni.so
//   cratonvm -cp /tmp/l6w101jni L6W101JniMonitorExitNotOwner /tmp/libl6w101jni.so
//   cratonvm --compatible -cp /tmp/l6w101jni L6W101JniMonitorExitNotOwner /tmp/libl6w101jni.so
// (Windows: cl /LD /I%JAVA_HOME%\include /I%JAVA_HOME%\include\win32 ... -> l6w101jni.dll)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// exit never entered: rc=-1 java.lang.IllegalMonitorStateException: current thread is not owner
// exit null: rc=-1 java.lang.NullPointerException
// enter null: rc=-1 java.lang.NullPointerException
// enter: rc=0 none
// holdsLock after enter: true
// exit: rc=0 none
// holdsLock after exit: false
// exit held by another thread: rc=-1 java.lang.IllegalMonitorStateException: current thread is not owner
// other still holds: true
// exit after synchronized released it: rc=-1 java.lang.IllegalMonitorStateException: current thread is not owner
// HOTSPOT_EXPECTED_END

import java.util.concurrent.CountDownLatch;

public class L6W101JniMonitorExitNotOwner {
    static native void enter(Object o);
    static native void exit(Object o);

    static String last;

    static void report(int rc, Throwable t) {
        last = "rc=" + rc + " " + (t == null ? "none" : String.valueOf(t));
    }

    public static void main(String[] args) throws Exception {
        System.load(args[0]);
        Object never = new Object();
        exit(never);
        System.out.println("exit never entered: " + last);
        exit(null);
        System.out.println("exit null: " + last);
        enter(null);
        System.out.println("enter null: " + last);

        Object o = new Object();
        enter(o);
        System.out.println("enter: " + last);
        System.out.println("holdsLock after enter: " + Thread.holdsLock(o));
        exit(o);
        System.out.println("exit: " + last);
        System.out.println("holdsLock after exit: " + Thread.holdsLock(o));

        Object held = new Object();
        CountDownLatch locked = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(1);
        boolean[] stillHolds = new boolean[1];
        Thread other = new Thread(() -> {
            synchronized (held) {
                locked.countDown();
                try {
                    done.await();
                } catch (InterruptedException e) {
                    return;
                }
                stillHolds[0] = Thread.holdsLock(held);
            }
        }, "other");
        other.start();
        locked.await();
        exit(held);
        System.out.println("exit held by another thread: " + last);
        done.countDown();
        other.join();
        System.out.println("other still holds: " + stillHolds[0]);

        Object balanced = new Object();
        synchronized (balanced) {
            balanced.hashCode();
        }
        exit(balanced);
        System.out.println("exit after synchronized released it: " + last);
    }
}
