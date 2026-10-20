// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L3 (proposal
// `i37-L6-proposal-hidden-method-bit-at-link-time`, the cut half of step 3;
// `CRATONVM_HIDDEN_METHOD_BITS`, default off, performance).
//
// HotSpot's `fill_in_stack_trace` skips a hidden frame (a lambda proxy's
// `run`, a LambdaForm / DirectMethodHandle$Holder frame) before it counts it,
// so `MaxJavaStackTraceDepth` (1024) counts recorded frames only. CratonVM
// cuts first and drops hidden frames after; a cut capture that lost hidden
// frames is redone (whole with the switch off, in growing windows with it
// on). Every row recurses through hidden frames to a depth whose RAW frame
// count is below, around or far above the cap, then captures
// `new Throwable().getStackTrace()` and a default `StackWalker` walk (not
// capped) at the leaf, 30 times (so the JIT compiles and inlines the
// recursion), and prints one fingerprint per row: the frame count, a count per
// method, the innermost four and the outermost three frames with lines.
// `stable` says every repetition printed the same.
//
// Rows: `lambda-L` recurses through a `Runnable` lambda (per level: the
// method, the hidden proxy `run`, the lambda body); `handle-L` through
// `MethodHandle.invokeExact` (per level: the method plus HotSpot's hidden
// LambdaForm frames); `mixed-L` alternates both.
//
// Run: javac -d . L3W123HiddenDepthCut.java
//      java -cp . L3W123HiddenDepthCut                                  (HotSpot reference)
//      java -Xint -cp . L3W123HiddenDepthCut                            (same answer)
//      cratonvm --jdk-only -cp . L3W123HiddenDepthCut
//      cratonvm --compatible -cp . L3W123HiddenDepthCut
//      (JIT and --nojit, each with and without CRATONVM_HIDDEN_METHOD_BITS=1;
//      expected SAME in every arm. `--compatible` hides hidden frames only
//      under its own switch, so compare it against its usual baseline first.)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// lambda-300 stable=true
//   trace len=605 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=300, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=301, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
//   walk  len=605 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=300, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=301, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
// lambda-400 stable=true
//   trace len=805 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=400, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=401, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
//   walk  len=805 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=400, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=401, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
// lambda-515 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=511, L3W123HiddenDepthCut::viaLambda=512} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::viaLambda:96, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96]
//   walk  len=1035 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=515, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=516, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
// lambda-2000 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=511, L3W123HiddenDepthCut::viaLambda=512} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::viaLambda:96, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96]
//   walk  len=4005 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaLambda$0=2000, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaLambda=2001, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaLambda:92, L3W123HiddenDepthCut::lambda$viaLambda$0:95, L3W123HiddenDepthCut::viaLambda:96] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:182, java.lang.Thread::run:1474]
// handle-600 stable=true
//   trace len=605 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaHandle=601, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:185, java.lang.Thread::run:1474]
//   walk  len=605 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaHandle=601, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:185, java.lang.Thread::run:1474]
// handle-1030 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::viaHandle=1023} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104]
//   walk  len=1035 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaHandle=1031, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:185, java.lang.Thread::run:1474]
// handle-3000 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::viaHandle=1023} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104]
//   walk  len=3005 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaHandle=3001, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaHandle:101, L3W123HiddenDepthCut::viaHandle:104, L3W123HiddenDepthCut::viaHandle:104] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:185, java.lang.Thread::run:1474]
// mixed-700 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaMixed$0=341, L3W123HiddenDepthCut::viaMixed=682} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaMixed:109, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122] outer=[L3W123HiddenDepthCut::viaMixed:113, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122]
//   walk  len=1055 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaMixed$0=350, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaMixed=701, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaMixed:109, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:188, java.lang.Thread::run:1474]
// mixed-2400 stable=true
//   trace len=1024 methods={L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaMixed$0=341, L3W123HiddenDepthCut::viaMixed=682} inner=[L3W123HiddenDepthCut::capture:146, L3W123HiddenDepthCut::viaMixed:109, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122] outer=[L3W123HiddenDepthCut::viaMixed:113, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122]
//   walk  len=3605 methods={L3W123HiddenDepthCut$Body::run=1, L3W123HiddenDepthCut::capture=1, L3W123HiddenDepthCut::lambda$viaMixed$0=1200, L3W123HiddenDepthCut::row=1, L3W123HiddenDepthCut::viaMixed=2401, java.lang.Thread::run=1} inner=[L3W123HiddenDepthCut::capture:150, L3W123HiddenDepthCut::viaMixed:109, L3W123HiddenDepthCut::lambda$viaMixed$0:117, L3W123HiddenDepthCut::viaMixed:122] outer=[L3W123HiddenDepthCut::row:164, L3W123HiddenDepthCut$Body::run:188, java.lang.Thread::run:1474]
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

public class L3W123HiddenDepthCut {
    static final MethodHandle HANDLE;
    static final MethodHandle MIXED;

    static {
        try {
            MethodType type = MethodType.methodType(void.class, int.class, Runnable.class);
            HANDLE = MethodHandles.lookup().findStatic(L3W123HiddenDepthCut.class, "viaHandle", type);
            MIXED = MethodHandles.lookup().findStatic(L3W123HiddenDepthCut.class, "viaMixed", type);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static String traceFingerprint;
    static String walkFingerprint;

    static void viaLambda(int n, Runnable leaf) {
        if (n == 0) {
            leaf.run();
            return;
        }
        Runnable next = () -> viaLambda(n - 1, leaf);
        next.run();
    }

    static void viaHandle(int n, Runnable leaf) throws Throwable {
        if (n == 0) {
            leaf.run();
            return;
        }
        HANDLE.invokeExact(n - 1, leaf);
    }

    static void viaMixed(int n, Runnable leaf) throws Throwable {
        if (n == 0) {
            leaf.run();
            return;
        }
        if ((n & 1) == 0) {
            MIXED.invokeExact(n - 1, leaf);
        } else {
            Runnable next = () -> {
                try {
                    viaMixed(n - 1, leaf);
                } catch (Throwable t) {
                    throw new RuntimeException(t);
                }
            };
            next.run();
        }
    }

    static String name(String cls, String method, int line) {
        // A lambda body's synthetic name is stable for one javac; a hidden
        // class name (never expected here) would carry an address.
        return cls + "::" + method + ":" + line;
    }

    static String fingerprint(List<String> frames) {
        Map<String, Integer> perMethod = new TreeMap<>();
        for (String f : frames) {
            String key = f.substring(0, f.lastIndexOf(':'));
            perMethod.merge(key, 1, Integer::sum);
        }
        int n = frames.size();
        List<String> inner = frames.subList(0, Math.min(4, n));
        List<String> outer = frames.subList(Math.max(0, n - 3), n);
        return "len=" + n + " methods=" + perMethod + " inner=" + inner + " outer=" + outer;
    }

    static void capture() {
        List<String> trace = new ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            trace.add(name(e.getClassName(), e.getMethodName(), e.getLineNumber()));
        }
        traceFingerprint = fingerprint(trace);
        List<String> walk = StackWalker.getInstance().walk(s ->
                s.map(f -> name(f.getClassName(), f.getMethodName(), f.getLineNumber())).toList());
        walkFingerprint = fingerprint(walk);
    }

    interface Row {
        void run(int levels, Runnable leaf) throws Throwable;
    }

    static void row(String label, Row body, int levels) throws Throwable {
        String firstTrace = null;
        String firstWalk = null;
        boolean stable = true;
        for (int rep = 0; rep < 30; rep++) {
            body.run(levels, L3W123HiddenDepthCut::capture);
            if (firstTrace == null) {
                firstTrace = traceFingerprint;
                firstWalk = walkFingerprint;
            } else if (!firstTrace.equals(traceFingerprint) || !firstWalk.equals(walkFingerprint)) {
                stable = false;
            }
        }
        System.out.println(label + " stable=" + stable);
        System.out.println("  trace " + firstTrace);
        System.out.println("  walk  " + firstWalk);
    }

    static class Body implements Runnable {
        @Override
        public void run() {
            try {
                for (int levels : new int[] {300, 400, 515, 2000}) {
                    row("lambda-" + levels, L3W123HiddenDepthCut::viaLambda, levels);
                }
                for (int levels : new int[] {600, 1030, 3000}) {
                    row("handle-" + levels, L3W123HiddenDepthCut::viaHandle, levels);
                }
                for (int levels : new int[] {700, 2400}) {
                    row("mixed-" + levels, L3W123HiddenDepthCut::viaMixed, levels);
                }
            } catch (Throwable t) {
                System.out.println("FAILED " + t);
            }
        }
    }

    public static void main(String[] args) throws InterruptedException {
        Thread deep = new Thread(null, new Body(), "deep", 1L << 29);
        deep.start();
        deep.join();
    }
}
