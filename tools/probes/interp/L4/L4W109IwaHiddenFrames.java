// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 109, lane L4 (page
// `docs/known-issues/interpreter/gap-hidden-walks-lack-hotspots-spun-lambda-form-frames-20261016.md`,
// row `invokeWithArguments hidden` of `L3/L3W104WalkOptionsThroughHandles`):
// the frames a StackWalker sees inside a static (int)int target reached by
// `MethodHandle.invokeWithArguments`, both overloads, with and without
// SHOW_HIDDEN_FRAMES, plus the throwable trace (= the default walk).
//
// HotSpot runs the JDK body: asType(genericMethodType(1)) (a spun
// LambdaForm$MH adapter), then spreadInvoker(0).invokeExact (the
// Invokers$Holder linker and the spun spreader LambdaForm$MH). CratonVM
// serves invokeWithArguments with an Intrinsic and lists none of the two
// LambdaForm$MH frames; with `CRATONVM_MH_IWA_JDK_BODY=1` (default off) the
// JDK body runs and the frames are real activations.
//
// Hidden-class addresses are folded. The expected block is the HotSpot 25.0.3
// output the L3 page records for the array row, extended to the List
// overload (its body calls the array overload virtually).
//
// Run: javac -d . L4W109IwaHiddenFrames.java
//      java -cp . L4W109IwaHiddenFrames                                         (HotSpot)
//      CRATONVM_MH_IWA_JDK_BODY=1 cratonvm --jdk-only [--nojit] -cp . L4W109IwaHiddenFrames
//      CRATONVM_MH_IWA_JDK_BODY=1 cratonvm --compatible [--nojit] -cp . L4W109IwaHiddenFrames
//      (switch off: the two `hidden` rows lack both LambdaForm$MH::invoke frames;
//       every other row is SAME in both arms)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25)
// array default: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.MethodHandle::invokeWithArguments]
// array hidden: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments]
// array trace: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.MethodHandle::invokeWithArguments]
// array result: 10
// list default: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.MethodHandle::invokeWithArguments, java.lang.invoke.MethodHandle::invokeWithArguments]
// list hidden: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.LambdaForm$MH::invoke, java.lang.invoke.Invokers$Holder::invokeExact_MT, java.lang.invoke.MethodHandle::invokeWithArguments, java.lang.invoke.MethodHandle::invokeWithArguments]
// list trace: [L4W109IwaHiddenFrames::targetInt, java.lang.invoke.MethodHandle::invokeWithArguments, java.lang.invoke.MethodHandle::invokeWithArguments]
// list result: 12
// HOTSPOT_EXPECTED_END

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W109IwaHiddenFrames {
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static final StackWalker HIDDEN = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES);
    static String row = "?";

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("LambdaForm\\$(\\w+)/ADDR", "LambdaForm\\$$1");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                if (f.getClassName().equals(L4W109IwaHiddenFrames.class.getName())
                        && f.getMethodName().startsWith("row")) {
                    break;
                }
                out.add(fold(f.getClassName() + "::" + f.getMethodName()));
            }
            return out;
        });
    }

    static List<String> trace() {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            if (e.getClassName().equals(L4W109IwaHiddenFrames.class.getName())
                    && e.getMethodName().startsWith("row")) {
                break;
            }
            out.add(fold(e.getClassName() + "::" + e.getMethodName()));
        }
        return out;
    }

    static int targetInt(int x) {
        List<String> d = walk(DEFAULT);
        List<String> h = walk(HIDDEN);
        List<String> t = trace();
        // Drop the walking frames themselves (walk / trace and the lambda).
        System.out.println(row + " default: " + strip(d));
        System.out.println(row + " hidden: " + strip(h));
        System.out.println(row + " trace: " + strip(t));
        return x * 2;
    }

    static List<String> strip(List<String> frames) {
        List<String> out = new ArrayList<>();
        boolean seen = false;
        for (String f : frames) {
            if (f.endsWith("::targetInt")) {
                seen = true;
            }
            if (seen) {
                out.add(f);
            }
        }
        return out;
    }

    static MethodHandle handle() throws Throwable {
        return MethodHandles.lookup().findStatic(L4W109IwaHiddenFrames.class, "targetInt",
                MethodType.methodType(int.class, int.class));
    }

    static void rowArray() throws Throwable {
        row = "array";
        Object r = handle().invokeWithArguments(5);
        System.out.println("array result: " + r);
    }

    static void rowList() throws Throwable {
        row = "list";
        Object r = handle().invokeWithArguments(List.of(6));
        System.out.println("list result: " + r);
    }

    public static void main(String[] args) throws Throwable {
        rowArray();
        rowList();
    }
}
