// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 103, lane L6 -- the self-building form of
// `L6W101JniMonitorExitNotOwner` (page
// `i101-L6-jni-monitor-exit-not-owner-answers-ok`), so the battery runs it
// without a hand-built library: the probe writes its JNI shim (the C source
// below, the same shim as `L6W101JniMonitorExitNotOwner.c` under this class's
// name) into a scratch directory, compiles it with the first C compiler on
// PATH (`gcc`, `cc`, `clang`, then MSVC `cl` when a Developer prompt set it
// up) against `<java.home>/include` (else `$JAVA_HOME/include`), loads it and
// asks JNI `MonitorEnter` / `MonitorExit` what they leave pending. The first
// line says whether a library was built: a run whose first line is
// `native library: none` (no compiler, or no `jni.h`) proves nothing.
//
// HotSpot 25 (jni.cpp `jni_MonitorEnter` / `jni_MonitorExit`,
// synchronizer.cpp `ObjectSynchronizer::jni_exit`, objectMonitor.cpp
// `check_owner`): a null object is `JNI_ERR` with a `NullPointerException`
// (no message) pending; an exit of a monitor the caller does not own is
// `JNI_ERR` with `IllegalMonitorStateException: current thread is not owner`
// pending, and the other thread keeps its monitor.
//
// Run: javac -d out L6W103JniMonitorExitSelfBuilt.java
//      java -cp out L6W103JniMonitorExitSelfBuilt
//      cratonvm [--compatible] -cp out L6W103JniMonitorExitSelfBuilt
//      (optional arg 0: a prebuilt library path; then nothing is compiled)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// native library: built
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

import java.io.File;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L6W103JniMonitorExitSelfBuilt {
    static native void enter(Object o);
    static native void exit(Object o);

    static String last;

    static void report(int rc, Throwable t) {
        last = "rc=" + rc + " " + (t == null ? "none" : String.valueOf(t));
    }

    static final String SHIM = String.join("\n",
            "#include <jni.h>",
            "static void report(JNIEnv *env, jclass cls, jint rc) {",
            "    jthrowable t = (*env)->ExceptionOccurred(env);",
            "    if (t != NULL) {",
            "        (*env)->ExceptionClear(env);",
            "    }",
            "    jmethodID m = (*env)->GetStaticMethodID(env, cls, \"report\", \"(ILjava/lang/Throwable;)V\");",
            "    (*env)->CallStaticVoidMethod(env, cls, m, rc, t);",
            "}",
            "JNIEXPORT void JNICALL Java_L6W103JniMonitorExitSelfBuilt_enter(JNIEnv *env, jclass cls, jobject o) {",
            "    jint rc = (*env)->MonitorEnter(env, o);",
            "    report(env, cls, rc);",
            "}",
            "JNIEXPORT void JNICALL Java_L6W103JniMonitorExitSelfBuilt_exit(JNIEnv *env, jclass cls, jobject o) {",
            "    jint rc = (*env)->MonitorExit(env, o);",
            "    report(env, cls, rc);",
            "}",
            "");

    static File includeDir() {
        List<String> homes = new ArrayList<>();
        homes.add(System.getProperty("java.home"));
        String env = System.getenv("JAVA_HOME");
        if (env != null) {
            homes.add(env);
        }
        for (String h : homes) {
            if (h == null) {
                continue;
            }
            File inc = new File(h, "include");
            if (new File(inc, "jni.h").isFile()) {
                return inc;
            }
        }
        return null;
    }

    static boolean tryBuild(File dir, List<String> cmd, File lib) {
        try {
            Process p = new ProcessBuilder(cmd).directory(dir)
                    .redirectErrorStream(true)
                    .redirectOutput(new File(dir, "cc.log"))
                    .start();
            p.getOutputStream().close();
            if (!p.waitFor(180, TimeUnit.SECONDS)) {
                p.destroyForcibly();
                return false;
            }
            return p.exitValue() == 0 && lib.isFile();
        } catch (IOException | InterruptedException e) {
            return false;
        }
    }

    /** Builds the shim; answers the library path, or null. */
    static String build(File dir) throws IOException {
        File inc = includeDir();
        if (inc == null) {
            return null;
        }
        boolean windows = System.getProperty("os.name").startsWith("Windows");
        File md = new File(inc, windows ? "win32" : (System.getProperty("os.name").startsWith("Mac") ? "darwin" : "linux"));
        File src = new File(dir, "l6w103jni.c");
        Files.write(src.toPath(), SHIM.getBytes(StandardCharsets.US_ASCII));
        File lib = new File(dir, System.mapLibraryName("l6w103jni"));
        for (String cc : new String[] {"gcc", "cc", "clang"}) {
            List<String> cmd = new ArrayList<>(List.of(cc, "-shared"));
            if (!windows) {
                cmd.add("-fPIC");
            }
            cmd.addAll(List.of("-I" + inc.getPath(), "-I" + md.getPath(), "-o", lib.getPath(), src.getPath()));
            if (tryBuild(dir, cmd, lib)) {
                return lib.getPath();
            }
        }
        if (windows) {
            List<String> cmd = List.of("cl", "/nologo", "/LD", "/I" + inc.getPath(), "/I" + md.getPath(),
                    src.getPath(), "/Fe" + lib.getPath());
            if (tryBuild(dir, cmd, lib)) {
                return lib.getPath();
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        File dir = null;
        String lib;
        if (args.length > 0) {
            lib = args[0];
        } else {
            dir = Files.createTempDirectory("l6w103jni").toFile();
            lib = build(dir);
        }
        System.out.println("native library: " + (lib == null ? "none" : "built"));
        if (lib == null) {
            return;
        }
        System.load(lib);
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
        // The scratch directory is left behind: a loaded DLL cannot be
        // deleted on Windows while the process runs.
    }
}
