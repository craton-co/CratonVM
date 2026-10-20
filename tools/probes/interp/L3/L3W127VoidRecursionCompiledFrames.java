// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 127, lane L3
// (`interpreter-i127-L3-jit-deep-handle-recursion-loses-a-frame-FIXED-20261224.md`,
// `CRATONVM_JIT_OSR_DEDUPE_SPARES_INVOKE_CALLER`, parity, default ON).
//
// Under the JIT, an interpreted activation whose VOID self-call is followed by
// `return` (an empty operand stack, so the single-pass artifact publishes an
// OSR entry at that pc) lost its compiled recursive callee: the stack-walk
// dedupe's pc heuristic read the caller's `pc` -- already past the invoke --
// as an OSR back-edge and dropped the callee as its "continuation". One frame
// went missing at the interpreted -> compiled boundary, in
// `Throwable.getStackTrace` and `StackWalker` alike, and only once the method
// had compiled, so the answer moved between repetitions.
//
// Each row recurses 30 times to a depth below MaxJavaStackTraceDepth (so no
// cut is involved), captures both walks at the leaf, and prints the frame
// count and the count of the recursing method. `stable` says every
// repetition printed the same. `int-600` (a non-void self-call: the result is
// on the stack after the invoke, so no OSR entry there) is the control that
// was always right.
//
// Run: javac -d . L3W127VoidRecursionCompiledFrames.java
//      java -cp . L3W127VoidRecursionCompiledFrames              (HotSpot reference)
//      java -Xint -cp . L3W127VoidRecursionCompiledFrames        (same answer)
//      cratonvm --jdk-only -cp . L3W127VoidRecursionCompiledFrames
//      cratonvm --compatible -cp . L3W127VoidRecursionCompiledFrames
//      (JIT and --nojit; expected SAME in every arm. With
//      CRATONVM_JIT_OSR_DEDUPE_SPARES_INVOKE_CALLER=0 the void rows are
//      expected to DIFF under the JIT -- the negative control.)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; -Xint identical)
// static-600 stable=true trace len=607 viaStatic=601 walk len=607 viaStatic=601
// handle-600 stable=true trace len=607 viaHandle=601 walk len=607 viaHandle=601
// virtual-600 stable=true trace len=607 viaVirtual=601 walk len=607 viaVirtual=601
// lambda-300 stable=true trace len=607 viaLambda=301 walk len=607 viaLambda=301
// int-600 stable=true trace len=607 viaInt=601 walk len=607 viaInt=601
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class L3W127VoidRecursionCompiledFrames {
    static final MethodHandle HANDLE;

    static {
        try {
            HANDLE = MethodHandles.lookup().findStatic(L3W127VoidRecursionCompiledFrames.class,
                    "viaHandle", MethodType.methodType(void.class, int.class, Runnable.class));
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static String traceFingerprint;
    static String walkFingerprint;
    static String counted;

    static void viaStatic(int n, Runnable leaf) {
        if (n == 0) {
            leaf.run();
            return;
        }
        viaStatic(n - 1, leaf);
    }

    static void viaHandle(int n, Runnable leaf) throws Throwable {
        if (n == 0) {
            leaf.run();
            return;
        }
        HANDLE.invokeExact(n - 1, leaf);
    }

    void viaVirtual(int n, Runnable leaf) {
        if (n == 0) {
            leaf.run();
            return;
        }
        viaVirtual(n - 1, leaf);
    }

    static void viaLambda(int n, Runnable leaf) {
        if (n == 0) {
            leaf.run();
            return;
        }
        Runnable next = () -> viaLambda(n - 1, leaf);
        next.run();
    }

    static int viaInt(int n, Runnable leaf) {
        if (n == 0) {
            leaf.run();
            return 0;
        }
        return viaInt(n - 1, leaf);
    }

    static String fingerprint(List<String> methods) {
        int same = 0;
        for (String m : methods) {
            if (m.equals(counted)) {
                same++;
            }
        }
        return "len=" + methods.size() + " " + counted + "=" + same;
    }

    static void capture() {
        List<String> trace = new java.util.ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            trace.add(e.getMethodName());
        }
        traceFingerprint = fingerprint(trace);
        walkFingerprint = fingerprint(StackWalker.getInstance().walk(s ->
                s.map(StackWalker.StackFrame::getMethodName).toList()));
    }

    interface Body {
        void run(Runnable leaf) throws Throwable;
    }

    static void row(String label, String method, Body body) throws Throwable {
        counted = method;
        String firstTrace = null;
        String firstWalk = null;
        boolean stable = true;
        for (int rep = 0; rep < 30; rep++) {
            body.run(L3W127VoidRecursionCompiledFrames::capture);
            if (firstTrace == null) {
                firstTrace = traceFingerprint;
                firstWalk = walkFingerprint;
            } else if (!firstTrace.equals(traceFingerprint) || !firstWalk.equals(walkFingerprint)) {
                stable = false;
            }
        }
        System.out.println(label + " stable=" + stable + " trace " + firstTrace + " walk " + firstWalk);
    }

    static void rows() throws Throwable {
        L3W127VoidRecursionCompiledFrames self = new L3W127VoidRecursionCompiledFrames();
        row("static-600", "viaStatic", leaf -> viaStatic(600, leaf));
        row("handle-600", "viaHandle", leaf -> viaHandle(600, leaf));
        row("virtual-600", "viaVirtual", leaf -> self.viaVirtual(600, leaf));
        row("lambda-300", "viaLambda", leaf -> viaLambda(300, leaf));
        row("int-600", "viaInt", leaf -> viaInt(600, leaf));
    }

    public static void main(String[] args) throws InterruptedException {
        Thread deep = new Thread(null, () -> {
            try {
                rows();
            } catch (Throwable t) {
                System.out.println("FAILED " + t);
            }
        }, "deep", 1L << 28);
        deep.start();
        deep.join();
    }
}
