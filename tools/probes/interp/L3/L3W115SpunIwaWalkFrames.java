// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L3 (page
// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`): the frames a
// `StackWalker` with SHOW_HIDDEN_FRAMES (and RETAIN_CLASS_REFERENCE) lists
// inside a target reached by `MethodHandle.invokeWithArguments` on a direct
// handle. HotSpot runs the JDK body: the spread invoker's spun
// `LambdaForm$MH.invoke(Object,Object,Object)Object` and, when the handle's
// type is not `genericMethodType(n)`, the `asType` adapter's spun
// `LambdaForm$MH.invoke(Object x (n+1))Object`, between the
// `DirectMethodHandle$Holder` frame and `Invokers$Holder.invokeExact_MT`.
// `CRATONVM_STANDIN_SPUN_IWA_FRAMES=1` (default off) has the walk ask the
// JDK for those two spun classes and list their frames without running the
// JDK body for the call.
//
// Each frame: class::method methodType bci=<bci> line=<line> [H] when its
// declaring class is hidden. Spun-class addresses folded.
//
// Run: javac -d . L3W115SpunIwaWalkFrames.java
//      java -cp . L3W115SpunIwaWalkFrames                          (HotSpot reference)
//      cratonvm --jdk-only -cp . L3W115SpunIwaWalkFrames            (switch off: the
//          `hidden` rows lack the LambdaForm$MH frames, other rows SAME)
//      CRATONVM_STANDIN_SPUN_IWA_FRAMES=1 cratonvm --jdk-only -cp . L3W115SpunIwaWalkFrames
//      (and the same two with --compatible; JIT and --nojit)
//      Expected with the switch on: every row SAME.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// static-int hidden: L3W115SpunIwaWalkFrames::tInt(int)int bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,int)int bci=10 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object)Object bci=31 line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=45 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// static-int default: L3W115SpunIwaWalkFrames::tInt
// static-int result: 7
// static-obj hidden: L3W115SpunIwaWalkFrames::tObj(Object)Object bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,Object)Object bci=10 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=45 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// static-obj default: L3W115SpunIwaWalkFrames::tObj
// static-obj result: x
// static-int2 hidden: L3W115SpunIwaWalkFrames::tInt2(int,int)int bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,int,int)int bci=11 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=44 line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=52 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// static-int2 default: L3W115SpunIwaWalkFrames::tInt2
// static-int2 result: 3
// static-void hidden: L3W115SpunIwaWalkFrames::tVoid()void bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object)void bci=9 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object)Object bci=12 line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=33 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// static-void default: L3W115SpunIwaWalkFrames::tVoid
// static-void result: null
// static-str hidden: L3W115SpunIwaWalkFrames::tStr(String)String bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,Object)Object bci=10 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object)Object bci=31 line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=45 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// static-str default: L3W115SpunIwaWalkFrames::tStr
// static-str result: s!
// list-int hidden: L3W115SpunIwaWalkFrames::tInt(int)int bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,int)int bci=10 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object)Object bci=31 line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object bci=45 line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735 | java.lang.invoke.MethodHandle::invokeWithArguments(List)Object bci=7 line=760
// list-int default: L3W115SpunIwaWalkFrames::tInt
// list-int result: 7
// again-int hidden: L3W115SpunIwaWalkFrames::tInt(int)int bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,int)int bci=10 line=-1 | java.lang.invoke.LambdaForm$MH::invoke(Object,Object)Object line=-1 [H] | java.lang.invoke.LambdaForm$MH::invoke(Object,Object,Object)Object line=-1 [H] | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object,Object)Object bci=20 line=-1 | java.lang.invoke.MethodHandle::invokeWithArguments(Object[])Object bci=28 line=735
// again-int default: L3W115SpunIwaWalkFrames::tInt
// again-int result: 7
// invokeExact hidden: L3W115SpunIwaWalkFrames::tObj(Object)Object bci=0 line=-1 | java.lang.invoke.DirectMethodHandle$Holder::invokeStatic(Object,Object)Object bci=10 line=-1 | java.lang.invoke.Invokers$Holder::invokeExact_MT(Object,Object,Object)Object bci=19 line=-1
// invokeExact default: L3W115SpunIwaWalkFrames::tObj
// invokeExact result: y
// HOTSPOT_EXPECTED_END

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.EnumSet;
import java.util.List;

public class L3W115SpunIwaWalkFrames {
    static final StackWalker HIDDEN =
            StackWalker.getInstance(EnumSet.of(Option.SHOW_HIDDEN_FRAMES, Option.RETAIN_CLASS_REFERENCE));
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static String hiddenRow;
    static String defaultRow;
    /** After 127 calls the JDK customizes the handle's adapter (new bytecode): its bci is not compared. */
    static boolean noSpunBci;

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-fA-F]+", "$1ADDR")
                .replaceAll("LambdaForm\\$(\\w+)/ADDR", "LambdaForm\\$$1")
                .replace("java.lang.Object", "Object")
                .replace("java.lang.String", "String")
                .replace("java.util.List", "List");
    }

    static String frame(StackFrame f) {
        String type = f.getMethodType().toString();
        return fold(f.getClassName() + "::" + f.getMethodName() + type)
                + (noSpunBci && f.getDeclaringClass().isHidden() ? "" : " bci=" + f.getByteCodeIndex())
                + " line=" + (f.getClassName().startsWith("L3W115") ? -1 : f.getLineNumber())
                + (f.getDeclaringClass().isHidden() ? " [H]" : "");
    }

    /** From the target (the row's callee) down to the first frame of this class that is not a target. */
    static void capture() {
        hiddenRow = HIDDEN.walk(s -> {
            List<String> out = new ArrayList<>();
            boolean first = true;
            for (StackFrame f : (Iterable<StackFrame>) s.skip(1)::iterator) {
                boolean mine = f.getClassName().equals(L3W115SpunIwaWalkFrames.class.getName());
                if (mine && !first) break;
                String shown = frame(f);
                if (mine) {
                    // The target's own bci and line are not what the row compares.
                    shown = shown.replaceAll(" bci=\\d+", " bci=0");
                }
                out.add(shown);
                first = false;
            }
            return String.join(" | ", out);
        });
        defaultRow = DEFAULT.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s.skip(1)::iterator) {
                out.add(f.getClassName() + "::" + f.getMethodName());
                break;
            }
            return String.join(" | ", out);
        });
    }

    static int tInt(int x) { capture(); return x; }
    static Object tObj(Object x) { capture(); return x; }
    static int tInt2(int x, int y) { capture(); return x + y; }
    static void tVoid() { capture(); }
    static String tStr(String s) { capture(); return s + "!"; }

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body body) {
        hiddenRow = null;
        defaultRow = null;
        Object result;
        try {
            result = body.run();
        } catch (Throwable t) {
            result = t.getClass().getName();
        }
        System.out.println(name + " hidden: " + hiddenRow);
        System.out.println(name + " default: " + defaultRow);
        System.out.println(name + " result: " + result);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L3W115SpunIwaWalkFrames.class;
        MethodHandle hi = l.findStatic(me, "tInt", MethodType.methodType(int.class, int.class));
        MethodHandle ho = l.findStatic(me, "tObj", MethodType.methodType(Object.class, Object.class));
        MethodHandle hi2 = l.findStatic(me, "tInt2", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle hv = l.findStatic(me, "tVoid", MethodType.methodType(void.class));
        MethodHandle hs = l.findStatic(me, "tStr", MethodType.methodType(String.class, String.class));
        row("static-int", () -> hi.invokeWithArguments(7));
        row("static-obj", () -> ho.invokeWithArguments("x"));
        row("static-int2", () -> hi2.invokeWithArguments(1, 2));
        row("static-void", () -> hv.invokeWithArguments());
        row("static-str", () -> hs.invokeWithArguments("s"));
        row("list-int", () -> hi.invokeWithArguments(List.of(7)));
        for (int i = 0; i < 200; i++) {
            hi.invokeWithArguments(7);
        }
        noSpunBci = true;
        row("again-int", () -> hi.invokeWithArguments(7));
        noSpunBci = false;
        // Control: invokeExact of a pregenerated shape lists no spun frame.
        row("invokeExact", () -> (Object) ho.invokeExact((Object) "y"));
    }
}
