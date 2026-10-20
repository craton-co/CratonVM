// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 121, lane L3: the invoker an `invokeExact` call site links to, in a
// `SHOW_HIDDEN_FRAMES` walk, when `Invokers$Holder` does not pregenerate its shape
// (`CRATONVM_STANDIN_SPUN_LINKER_FRAMES`, default off; residual 5 of the gap page
// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames`, wave 121). HotSpot links such a site to
// a spun `LambdaForm$MH/0x...invokeExact_MT` (`Invokers.methodHandleInvokeLinkerMethod`); this VM
// served it with no frame there. Each row walks from inside a target reached by `invokeExact` and
// prints the frames between the target and the calling row, innermost first, as
// `class.method descriptor @bci hidden?` (a spun class's `/0x...` suffix cut). Pregenerated
// shapes are controls: their `Invokers$Holder.invokeExact_MT` frame was already listed.
//
// Run: cratonvm --java-home <jdk25> --jdk-only -cp <dir> L3W121SpunLinkerFrames
//      with and without CRATONVM_STANDIN_SPUN_LINKER_FRAMES=1; --compatible the same; JIT and
//      --nojit. Expected, switch off: `Object(Object)` SAME, the five others DIFF (no linker frame).
//      Switch on: `int(int)`, `void()`, `int(int,int)` SAME too. `long(long)` and
//      `double(String,double)` stay DIFF: HotSpot also spins their `LambdaForm$DMH` holder, which
//      the VM does not name, and the step declines over a bare target (fail-closed).
//      CRATONVM_DEBUG_STACKWALK=1 prints `[SW-DBG] spun-linker ...` lines for each decision.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// int(int) r=42: java.lang.invoke.DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;I)I @10 | java.lang.invoke.LambdaForm$MH.invokeExact_MT (Ljava/lang/Object;ILjava/lang/Object;)I @19 hidden
// long(long) r=42: java.lang.invoke.LambdaForm$DMH.invokeStatic (Ljava/lang/Object;J)J @10 hidden | java.lang.invoke.LambdaForm$MH.invokeExact_MT (Ljava/lang/Object;JLjava/lang/Object;)J @19 hidden
// Object(Object) r=o: java.lang.invoke.DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @10 | java.lang.invoke.Invokers$Holder.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object; @19
// void() : java.lang.invoke.DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;)V @9 | java.lang.invoke.LambdaForm$MH.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;)V @18 hidden
// int(int,int) r=42: java.lang.invoke.DirectMethodHandle$Holder.invokeStatic (Ljava/lang/Object;II)I @11 | java.lang.invoke.LambdaForm$MH.invokeExact_MT (Ljava/lang/Object;IILjava/lang/Object;)I @20 hidden
// double(String,double) r=2.5: java.lang.invoke.LambdaForm$DMH.invokeStatic (Ljava/lang/Object;Ljava/lang/Object;D)D @13 hidden | java.lang.invoke.LambdaForm$MH.invokeExact_MT (Ljava/lang/Object;Ljava/lang/Object;DLjava/lang/Object;)D @21 hidden
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;
import java.util.stream.Collectors;

public class L3W121SpunLinkerFrames {
    static final StackWalker HIDDEN = StackWalker.getInstance(
            java.util.Set.of(StackWalker.Option.SHOW_HIDDEN_FRAMES, StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static String frames() {
        List<String> out = HIDDEN.walk(s -> s.skip(2)
                .takeWhile(f -> !f.getMethodName().startsWith("row"))
                .map(f -> {
                    String cls = f.getClassName();
                    int cut = cls.indexOf("/0x");
                    if (cut >= 0) {
                        cls = cls.substring(0, cut);
                    }
                    return cls + "." + f.getMethodName() + " " + f.getDescriptor() + " @"
                            + f.getByteCodeIndex() + (f.getDeclaringClass().isHidden() ? " hidden" : "");
                })
                .collect(Collectors.toList()));
        return String.join(" | ", out);
    }

    static String seen;

    static int tInt(int x) {
        seen = frames();
        return x + 1;
    }

    static long tLong(long x) {
        seen = frames();
        return x + 1;
    }

    static Object tObj(Object x) {
        seen = frames();
        return x;
    }

    static void tVoid() {
        seen = frames();
    }

    static int tIntInt(int a, int b) {
        seen = frames();
        return a + b;
    }

    static double tStrDouble(String s, double d) {
        seen = frames();
        return d;
    }

    static MethodHandle find(String name, MethodType type) throws ReflectiveOperationException {
        return MethodHandles.lookup().findStatic(L3W121SpunLinkerFrames.class, name, type);
    }

    static void rowInt(MethodHandle h) throws Throwable {
        int r = (int) h.invokeExact(41);
        System.out.println("int(int) r=" + r + ": " + seen);
    }

    static void rowLong(MethodHandle h) throws Throwable {
        long r = (long) h.invokeExact(41L);
        System.out.println("long(long) r=" + r + ": " + seen);
    }

    static void rowObj(MethodHandle h) throws Throwable {
        Object r = (Object) h.invokeExact((Object) "o");
        System.out.println("Object(Object) r=" + r + ": " + seen);
    }

    static void rowVoid(MethodHandle h) throws Throwable {
        h.invokeExact();
        System.out.println("void() : " + seen);
    }

    static void rowIntInt(MethodHandle h) throws Throwable {
        int r = (int) h.invokeExact(40, 2);
        System.out.println("int(int,int) r=" + r + ": " + seen);
    }

    static void rowStrDouble(MethodHandle h) throws Throwable {
        double r = (double) h.invokeExact("s", 2.5);
        System.out.println("double(String,double) r=" + r + ": " + seen);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle hInt = find("tInt", MethodType.methodType(int.class, int.class));
        MethodHandle hLong = find("tLong", MethodType.methodType(long.class, long.class));
        MethodHandle hObj = find("tObj", MethodType.methodType(Object.class, Object.class));
        MethodHandle hVoid = find("tVoid", MethodType.methodType(void.class));
        MethodHandle hIntInt = find("tIntInt", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle hStrDouble = find("tStrDouble",
                MethodType.methodType(double.class, String.class, double.class));
        rowInt(hInt);
        rowLong(hLong);
        rowObj(hObj);
        rowVoid(hVoid);
        rowIntInt(hIntInt);
        rowStrDouble(hStrDouble);
    }
}
