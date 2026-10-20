// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L2 -- how many frames ANOTHER thread's
// `Thread.getStackTrace()` lists. HotSpot answers it from
// `java_lang_Thread::async_get_stack_trace`, whose
// `GetStackTraceHandshakeClosure` stops at `MaxJavaStackTraceDepth` frames
// (default 1024, `0` = all; the innermost ones, counted after the hidden-frame
// skip). The VM's `Thread.getStackTrace0` native listed every frame. The
// current thread's own `getStackTrace()` (`new Exception().getStackTrace()`)
// and `Thread.getAllStackTraces()` (`VM_ThreadDump`, whole stack) are the
// controls.
//
// A worker recurses 1500 deep and parks; main reads its stack three ways.
//
// Run (stdout must be the block for the run, byte for byte):
//   $J/javac -d out tools/probes/interp/L2/L2W84OtherThreadTraceDepth.java
//   $B --java-home $J/.. -cp out L2W84OtherThreadTraceDepth                     -> DEFAULT
//   $B --compatible --java-home $J/.. -cp out L2W84OtherThreadTraceDepth        -> DEFAULT
//   $B -XX:MaxJavaStackTraceDepth=0 --java-home $J/.. -cp out L2W84OtherThreadTraceDepth unlimited -> UNLIMITED
//   (each also with --nojit)
//   $J/java -cp out L2W84OtherThreadTraceDepth                                  -> DEFAULT
//   $J/java -XX:MaxJavaStackTraceDepth=0 -cp out L2W84OtherThreadTraceDepth unlimited -> UNLIMITED
//
// Derived from the JDK 25 sources named above; refresh from $J/java before
// reading a DIFF.
//
// HOTSPOT_EXPECTED_BEGIN DEFAULT (JDK 25, default flags)
// other-length 1024
// other-top recurse
// self-length 1024
// all-over-1500 true
// HOTSPOT_EXPECTED_END
//
// HOTSPOT_EXPECTED_BEGIN UNLIMITED (-XX:MaxJavaStackTraceDepth=0)
// other-length-over-1500 true
// other-top recurse
// self-length-over-1500 true
// all-over-1500 true
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CountDownLatch;

public class L2W84OtherThreadTraceDepth {
    static final int DEPTH = 1500;
    static final CountDownLatch parked = new CountDownLatch(1);
    static final CountDownLatch release = new CountDownLatch(1);
    static int selfLength;

    static void recurse(int n, boolean self) throws InterruptedException {
        if (n > 0) {
            recurse(n - 1, self);
            return;
        }
        if (self) {
            selfLength = Thread.currentThread().getStackTrace().length;
            return;
        }
        parked.countDown();
        release.await();
    }

    public static void main(String[] args) throws Exception {
        boolean unlimited = args.length > 0 && args[0].equals("unlimited");
        Thread worker = new Thread(() -> {
            try {
                recurse(DEPTH, false);
            } catch (InterruptedException e) {
                throw new RuntimeException(e);
            }
        }, "deep");
        worker.start();
        parked.await();
        StackTraceElement[] other = worker.getStackTrace();
        StackTraceElement[] all = Thread.getAllStackTraces().get(worker);
        release.countDown();
        worker.join();

        if (unlimited) {
            System.out.println("other-length-over-1500 " + (other.length > DEPTH));
        } else {
            System.out.println("other-length " + other.length);
        }
        // The innermost frames are the ones kept: the parked call's own frames
        // (CountDownLatch.await and below) come first; the first frame of
        // this class is the deepest `recurse`.
        String top = "none";
        for (StackTraceElement e : other) {
            if (e.getClassName().equals(L2W84OtherThreadTraceDepth.class.getName())) {
                top = e.getMethodName();
                break;
            }
        }
        System.out.println("other-top " + top);

        recurse(DEPTH, true);
        if (unlimited) {
            System.out.println("self-length-over-1500 " + (selfLength > DEPTH));
        } else {
            System.out.println("self-length " + selfLength);
        }
        System.out.println("all-over-1500 " + (all != null && all.length > DEPTH));
    }
}
