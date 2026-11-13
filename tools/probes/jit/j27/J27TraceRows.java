// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 4 (lane TRACEROWS): Java-visible stack answers from compiled code.
// Page: docs/feature-designs/jit-ir-optimizations.md
// (traces / stack walk / threads rows; TR16-5's full-frame JDK-raise battery).
//
// Rows (one `fact` line each, deterministic; timing lines only with
// -Dj27.timing=true):
//   deep.cold / deep.hot  an exception thrown in d() under a()->b()->c()->d(),
//                         first call and after 20 000 warm calls (the callees
//                         are tiny, so the hot one is thrown from an inlined
//                         callee of a compiled caller): every frame, method:line;
//   walk.off / walk.on    StackWalker with RETAIN_CLASS_REFERENCE, with
//                         SHOW_HIDDEN_FRAMES off and on, walked from inside a
//                         lambda: frames, lambda$ method frames, hidden-class
//                         frames, the top frame's getDeclaringClass;
//   walk.hot              the same two walks 3 000 times each (compiled
//                         callers), every answer equal to the cold one;
//   spin                  Thread.getStackTrace() of a worker spinning in a
//                         compiled (OSR) loop: every frame, probe frames with
//                         their line;
//   raise.<kind>          TR16-5: the full frame list of four common JDK raises
//                         (Integer.parseInt, ArrayList.get, Optional.get, a
//                         ConcurrentModificationException), JDK frames as
//                         Class.method, probe frames as method:line, plus the
//                         message, cold and after 20 000 caught raises.
//
//   javac -d <dir> J27TraceRows.java
//   java -cp <dir> J27TraceRows > hs.txt
//   cratonvm --cp <dir> J27TraceRows | diff - hs.txt
//     x {--real-jdk, --compatible} x {default, --nojit}
//
// Expected HotSpot 25 output (facts; the JDK frame lists are read from the
// JDK 25 sources, confirm them once against a HotSpot run on the host):
//   fact deep.cold d:76,c:72,b:68,a:64,deepRow:87,main:230
//   fact deep.hot d:76,c:72,b:68,a:64,deepRow:92,main:230
//   fact walk.off frames=4 lambdaFrames=1 hiddenFrames=0 top=J27TraceRows topLine=102
//   fact walk.on frames=5 lambdaFrames=1 hiddenFrames=1 top=J27TraceRows topLine=102
//   fact walk.hot same=true
//   fact spin spin:141,workerBody:136,java.lang.Thread.run
//   fact raise.parseInt.cold java.lang.NumberFormatException.forInputString,java.lang.Integer.parseInt,java.lang.Integer.parseInt,lambda:198,raise:190,raiseTrace:168,raiseRow:180,raises:202,main:233 msg=For input string: "x"
//   fact raise.parseInt.hot same=true
//   fact raise.listGet.cold jdk.internal.util.Preconditions.outOfBounds,jdk.internal.util.Preconditions.outOfBoundsCheckIndex,jdk.internal.util.Preconditions.checkIndex,java.util.Objects.checkIndex,java.util.ArrayList.get,lambda:199,raise:190,raiseTrace:168,raiseRow:180,raises:202,main:233 msg=Index 5 out of bounds for length 1
//   fact raise.listGet.hot same=true
//   fact raise.optionalGet.cold java.util.Optional.get,lambda:200,raise:190,raiseTrace:168,raiseRow:180,raises:202,main:233 msg=No value present
//   fact raise.optionalGet.hot same=true
//   fact raise.cme.cold java.util.ArrayList$Itr.checkForComodification,java.util.ArrayList$Itr.next,cme:207,lambda:201,raise:190,raiseTrace:168,raiseRow:180,raises:202,main:233 msg=null
//   fact raise.cme.hot same=true
//
// The hidden frames HotSpot leaves out of every trace above, and the walk.off
// row: the lambda proxies' run()/accept(), Thread.runWith (@Hidden), and on
// this VM the compiled indy bridge's <jit-indy>.concat frame under d()'s
// "deep " + x (TR16-2 / TR16-3).

import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import java.util.function.IntConsumer;

public class J27TraceRows {
    static int a(int x) {
        return b(x) + 1;
    }

    static int b(int x) {
        return c(x) + 1;
    }

    static int c(int x) {
        return d(x) + 1;
    }

    static int d(int x) {
        if (x < 0) throw new IllegalStateException("deep " + x);
        return x + 1;
    }

    static int sink;

    // The cold throw, then 20 000 warm calls, then the hot throw (the cold and
    // hot throws are on their own lines: those lines are in the answer).
    static void deepRow() {
        String cold = "";
        try {
            sink += a(-1);
        } catch (IllegalStateException e) { cold = frames(e.getStackTrace()); }
        for (int i = 0; i < 20_000; i++) sink += a(i);
        String hot = "";
        try {
            sink += a(-2);
        } catch (IllegalStateException e) { hot = frames(e.getStackTrace()); }
        System.out.println("fact deep.cold " + cold);
        System.out.println("fact deep.hot " + hot);
    }

    // The walk is taken from inside a lambda: its synthetic lambda$ method
    // is an ordinary frame; the proxy class's run() is a hidden frame.
    static String walkIn(StackWalker w) {
        List<StackWalker.StackFrame>[] box = new List[1];
        Runnable r = () -> box[0] = w.walk(s -> s.toList());
        r.run();
        List<StackWalker.StackFrame> fs = box[0];
        int lambdaFrames = 0;
        int hiddenFrames = 0;
        for (StackWalker.StackFrame f : fs) {
            if (f.getMethodName().startsWith("lambda$")) lambdaFrames++;
            if (f.getDeclaringClass().isHidden()) hiddenFrames++;
        }
        StackWalker.StackFrame top = fs.get(0);
        return "frames=" + fs.size() + " lambdaFrames=" + lambdaFrames + " hiddenFrames="
                + hiddenFrames + " top=" + top.getDeclaringClass().getName() + " topLine="
                + top.getLineNumber();
    }

    static void walkRow() {
        StackWalker off = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
        StackWalker on = StackWalker.getInstance(Set.of(
                StackWalker.Option.RETAIN_CLASS_REFERENCE, StackWalker.Option.SHOW_HIDDEN_FRAMES));
        String coldOff = walkIn(off);
        String coldOn = walkIn(on);
        System.out.println("fact walk.off " + coldOff);
        System.out.println("fact walk.on " + coldOn);
        boolean same = true;
        for (int i = 0; i < 3_000; i++) {
            same &= walkIn(off).equals(coldOff) && walkIn(on).equals(coldOn);
        }
        System.out.println("fact walk.hot same=" + same);
    }

    static volatile boolean stop;
    static volatile long progress;

    static void workerBody() {
        spin();
    }

    static void spin() {
        long n = 0;
        while (!stop) { n++; if ((n & 0xFFFF) == 0) progress = n; }
        sink += (int) n;
    }

    static void spinRow() throws InterruptedException {
        Thread worker = new Thread(J27TraceRows::workerBody, "j27-spin");
        worker.setDaemon(true);
        worker.start();
        long deadline = System.nanoTime() + 60_000_000_000L;
        while (progress < (1L << 26) && System.nanoTime() < deadline) Thread.sleep(5);
        // Several reads: each must name the loop (no read may see the worker
        // anywhere else, nor list a VM frame).
        String first = frames(worker.getStackTrace());
        boolean same = true;
        for (int i = 0; i < 20; i++) {
            same &= frames(worker.getStackTrace()).equals(first);
            Thread.sleep(1);
        }
        stop = true;
        worker.join();
        System.out.println("fact spin " + first + (same ? "" : " UNSTABLE"));
    }

    // TR16-5. Each raise is called through `raise`, cold once, then caught
    // 20 000 times, then once more; the hot trace must equal the cold one.
    static String raiseTrace(IntConsumer raise, int arg) {
        try {
            raise(raise, arg);
            return "no-throw";
        } catch (RuntimeException e) {
            return frames(e.getStackTrace()) + " msg=" + e.getMessage();
        }
    }

    static void raiseRow(String kind, IntConsumer body) {
        // Cold (pass 0) and hot (pass 1) traces are taken on ONE line.
        String[] t = new String[2];
        for (int pass = 0; pass < 2; pass++) {
            for (int i = 0; pass == 1 && i < 20_000; i++) raiseTrace(body, i);
            t[pass] = raiseTrace(body, 0);
        }
        String cold = t[0];
        String hot = t[1];
        System.out.println("fact raise." + kind + ".cold " + cold);
        System.out.println("fact raise." + kind + ".hot same=" + hot.equals(cold)
                + (hot.equals(cold) ? "" : " hot=" + hot));
    }

    static void raise(IntConsumer body, int arg) {
        body.accept(arg);
    }

    static final List<Integer> ONE = new ArrayList<>(List.of(7));

    static void raises() {
        String[] kinds = {"parseInt", "listGet", "optionalGet", "cme"};
        IntConsumer[] bodies = new IntConsumer[4];
        bodies[0] = x -> sink += Integer.parseInt("x");
        bodies[1] = x -> sink += ONE.get(5);
        bodies[2] = x -> sink += Optional.<Integer>empty().get();
        bodies[3] = x -> sink += cme();
        for (int k = 0; k < 4; k++) raiseRow(kinds[k], bodies[k]);
    }

    static int cme() {
        List<Integer> l = new ArrayList<>(List.of(1, 2));
        for (Integer i : l) l.add(i);
        return l.size();
    }

    // JDK frames as Class.method, this probe's as method:line (a lambda body's
    // synthetic method as `lambda`, whatever number javac gave it).
    static String frames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            if (sb.length() > 0) sb.append(',');
            if (e.getClassName().equals("J27TraceRows")) {
                String m = e.getMethodName();
                sb.append(m.startsWith("lambda$") ? "lambda" : m).append(':').append(e.getLineNumber());
            } else {
                sb.append(e.getClassName()).append('.').append(e.getMethodName());
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        boolean timing = Boolean.getBoolean("j27.timing");
        long t0 = System.nanoTime();
        deepRow();
        walkRow();
        spinRow();
        raises();
        if (timing) System.out.println("time total_ms=" + (System.nanoTime() - t0) / 1_000_000);
    }
}
