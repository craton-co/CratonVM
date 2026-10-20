// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L4: the varargs / collector / spreader
// adapters of a REAL JDK `MethodHandle` (one the JDK's own
// `DirectMethodHandle.make` built, with none of CratonVM's synthetic slots),
// and a real `MethodHandleImpl$AsVarargsCollector` invoked at a collecting
// arity. Proposal
// `docs/internal/fixed-bugs/interpreter-i54-L4-proposal-every-adapter-native-hands-a-real-handle-to-the-jdk-KEPT-OFF-20261019.md`.
// CratonVM before wave 55 (code read): `asVarargsCollector` answered the
// receiver itself (its width-guarded marking write was a no-op), the
// collector / spreader minted a carrier over a handle of no descriptor, and a
// real `AsVarargsCollector` at another arity raised
// `InternalError: CratonVM cannot invoke a ...`.
// Switch: `CRATONVM_MH_FOREIGN_VARARGS_RUN_JDK` (on in the lane's last
// commit). Positive control (`CRATONVM_DBG_MH_DISPATCH=1`):
// `[MH_FOREIGN] asVarargsCollector of java/lang/invoke/DirectMethodHandle
// through the JDK's own body`, `... asCollector of ...`, `... asSpreader of
// ...`, and `[MH_FOREIGN] AsVarargsCollector at (III)I: asType through the
// JDK's own body` for `avc-invoke-3`.
//
// Needs JVM flags: --add-opens java.base/java.lang.invoke=ALL-UNNAMED
// (it builds the real handles reflectively through `DirectMethodHandle.make`).
//
// Run: javac -d out L4W55RealJdkAdapters.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W55RealJdkAdapters
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W55RealJdkAdapters)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   avc-invoke-3-first: 6
//   avc-class: java.lang.invoke.MethodHandleImpl$AsVarargsCollector
//   avc-is-varargs: true
//   avc-type: (int[])int
//   avc-invoke-3: 6
//   avc-invoke-0: 0
//   avc-invoke-array: 9
//   avc-invoke-object: 3 Integer
//   avc-iwa: 3
//   avc-fixed-is-receiver: true
//   receiver-is-varargs: false
//   receiver-type-after: (int[])int
//   withvarargs-true: true (int[])int
//   withvarargs-false-is-receiver: true
//   avc-not-array: java.lang.IllegalArgumentException: not an array type: int
//   collector-type: (int,int)int
//   collector-invoke: 7
//   collector-at-type: (int,int,int)int
//   collector-at-invoke: 30
//   spreader-type: (int[])int
//   spreader-invoke: 7
//   spreader-at-type: (int[],int)int
//   spreader-at-invoke: 123
//   spreader-bad-length: java.lang.IllegalArgumentException: bad spread array length
//   avc-invoke-3-again: 6
import java.lang.invoke.MethodHandle;
import java.lang.reflect.Method;

public class L4W55RealJdkAdapters {
    static int sum(int[] xs) {
        int s = 0;
        for (int x : xs) {
            s += x;
        }
        return s;
    }

    static int weigh(int[] xs, int k) {
        return sum(xs) * k;
    }

    static int add(int a, int b) {
        return a + b;
    }

    static int add3(int a, int b, int c) {
        return a * 100 + b * 10 + c;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static MethodHandle realDirect(Method m) throws Throwable {
        Class<?> memberName = Class.forName("java.lang.invoke.MemberName");
        java.lang.reflect.Constructor<?> ctor = memberName.getDeclaredConstructor(Method.class);
        ctor.setAccessible(true);
        Object member = ctor.newInstance(m);
        Method make = Class.forName("java.lang.invoke.DirectMethodHandle").getDeclaredMethod("make", memberName);
        make.setAccessible(true);
        return (MethodHandle) make.invoke(null, member);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W55RealJdkAdapters.class;
        MethodHandle dSum = realDirect(me.getDeclaredMethod("sum", int[].class));
        MethodHandle dWeigh = realDirect(me.getDeclaredMethod("weigh", int[].class, int.class));
        MethodHandle dAdd = realDirect(me.getDeclaredMethod("add", int.class, int.class));
        MethodHandle dAdd3 = realDirect(me.getDeclaredMethod("add3", int.class, int.class, int.class));

        row("avc-invoke-3-first", () -> (int) dSum.asVarargsCollector(int[].class).invoke(1, 2, 3));
        MethodHandle avc = dSum.asVarargsCollector(int[].class);
        row("avc-class", () -> avc.getClass().getName());
        row("avc-is-varargs", () -> avc.isVarargsCollector());
        row("avc-type", () -> avc.type());
        row("avc-invoke-3", () -> (int) avc.invoke(1, 2, 3));
        row("avc-invoke-0", () -> (int) avc.invoke());
        row("avc-invoke-array", () -> (int) avc.invoke(new int[] {4, 5}));
        row("avc-invoke-object", () -> {
            Object r = avc.invoke(1, 2);
            return r + " " + r.getClass().getSimpleName();
        });
        row("avc-iwa", () -> avc.invokeWithArguments(1, 2));
        row("avc-fixed-is-receiver", () -> avc.asFixedArity() == dSum);
        row("receiver-is-varargs", () -> dSum.isVarargsCollector());
        row("receiver-type-after", () -> dSum.type());
        row("withvarargs-true", () -> {
            MethodHandle w = dSum.withVarargs(true);
            return w.isVarargsCollector() + " " + w.type();
        });
        row("withvarargs-false-is-receiver", () -> dSum.withVarargs(false) == dSum);
        row("avc-not-array", () -> dAdd.asVarargsCollector(int.class));
        MethodHandle collector = dSum.asCollector(int[].class, 2);
        row("collector-type", () -> collector.type());
        row("collector-invoke", () -> (int) collector.invokeExact(3, 4));
        MethodHandle collectorAt = dWeigh.asCollector(0, int[].class, 2);
        row("collector-at-type", () -> collectorAt.type());
        row("collector-at-invoke", () -> (int) collectorAt.invokeExact(1, 2, 10));
        MethodHandle spreader = dAdd.asSpreader(int[].class, 2);
        row("spreader-type", () -> spreader.type());
        row("spreader-invoke", () -> (int) spreader.invokeExact(new int[] {3, 4}));
        MethodHandle spreaderAt = dAdd3.asSpreader(0, int[].class, 2);
        row("spreader-at-type", () -> spreaderAt.type());
        row("spreader-at-invoke", () -> (int) spreaderAt.invokeExact(new int[] {1, 2}, 3));
        row("spreader-bad-length", () -> dAdd.asSpreader(int[].class, 3));
        row("avc-invoke-3-again", () -> (int) avc.invoke(1, 2, 3));
    }
}
