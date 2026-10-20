// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L6 --
// docs/internal/fixed-bugs/interpreter-i59-L6-compatible-synthetic-process-lists-no-jdk-frames-FIXED-20261115.md
//
// A child process is the JDK's own on HotSpot: ProcessBuilder.start() returns a
// java.lang.ProcessImpl whose streams are its nested ProcessPipe*Stream
// classes, a waitFor() thread parks under ProcessImpl.waitFor and a reader of
// the child's stdout is in the native FileInputStream.readBytes. --compatible
// returns the VM's cratonvm/synthetic/Process unless the new JdkProcess row is
// on. Linux host (`sleep 30` child).
//
// Run:
//   cratonvm L6W80CompatJdkProcess                                        (matches)
//   cratonvm --compatible L6W80CompatJdkProcess                           (row off: classes/frames differ)
//   CRATONVM_COMPAT_JDK_PROCESS=1 cratonvm --compatible L6W80CompatJdkProcess   (row on: must match)
//   (CRATONVM_COMPAT_ALIGN=+jdk-process is the same switch.)
// The exitValue row matches in every mode since wave 80 (the stand-in's text).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// process: java.lang.ProcessImpl
// stdout: java.lang.ProcessImpl$ProcessPipeInputStream
// stdin: java.lang.ProcessImpl$ProcessPipeOutputStream
// handle: java.lang.ProcessHandleImpl
// exitValue: java.lang.IllegalThreadStateException: process hasn't exited
// waitFor thread: WAITING under-ProcessImpl.waitFor=true
// reader thread: RUNNABLE in-readBytes=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.TimeUnit;

public class L6W80CompatJdkProcess {
    static boolean hasFrame(Thread t, String cls, String method) {
        for (StackTraceElement e : t.getStackTrace()) {
            if (e.getClassName().equals(cls) && e.getMethodName().equals(method)) {
                return true;
            }
        }
        return false;
    }

    static void settle(Thread t, Thread.State want) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (t.getState() != want && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(100);
    }

    public static void main(String[] args) throws Exception {
        Process p = new ProcessBuilder("sleep", "30").start();
        System.out.println("process: " + p.getClass().getName());
        System.out.println("stdout: " + p.getInputStream().getClass().getName());
        System.out.println("stdin: " + p.getOutputStream().getClass().getName());
        System.out.println("handle: " + p.toHandle().getClass().getName());
        try {
            System.out.println("exitValue: " + p.exitValue());
        } catch (IllegalThreadStateException e) {
            System.out.println("exitValue: " + e);
        }

        Thread waiter = new Thread(() -> {
            try {
                p.waitFor();
            } catch (InterruptedException e) {
                // the end of the row
            }
        }, "w80-waiter");
        waiter.setDaemon(true);
        waiter.start();
        settle(waiter, Thread.State.WAITING);
        System.out.println("waitFor thread: " + waiter.getState()
                + " under-ProcessImpl.waitFor=" + hasFrame(waiter, "java.lang.ProcessImpl", "waitFor"));

        Thread reader = new Thread(() -> {
            try {
                p.getInputStream().read();
            } catch (Exception e) {
                // the child was destroyed
            }
        }, "w80-reader");
        reader.setDaemon(true);
        reader.start();
        Thread.sleep(300);
        System.out.println("reader thread: " + reader.getState()
                + " in-readBytes=" + hasFrame(reader, "java.io.FileInputStream", "readBytes"));

        p.destroyForcibly();
        p.waitFor();
        waiter.join(2000);
        reader.join(2000);
        System.out.println("done");
    }
}
