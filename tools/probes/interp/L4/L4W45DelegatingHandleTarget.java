// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L4
// (`i44-L4-proposal-invoke-a-jdk-delegating-handle-through-its-target`,
// step 1, and the step-2 question: does a JDK `DelegatingMethodHandle` over
// one of CratonVM's handles CONSTRUCT at all?). The JDK builders are reached
// directly through reflection on `java.lang.invoke`, so nothing CratonVM pins
// (`arrayElementGetter`, `identity`) stands in front of them:
//
//   * `MethodHandleImpl.makeIntrinsic(target, Intrinsic)` wraps a
//     `findStatic` handle in a `MethodHandleImpl$IntrinsicMethodHandle`;
//   * `MethodHandleImpl.makeArrayElementAccessor(int[].class, GET)` is the
//     body of `arrayElementGetter` (`viewAsType`, then `makeIntrinsic`).
//
// Each row prints the handle's class, then invokes it through every door
// (`invokeExact`, `invoke`, `invokeWithArguments`, `insertArguments` over it
// as a combinator target, an exact invoker).
//
// Run: javac -d out L4W45DelegatingHandleTarget.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W45DelegatingHandleTarget
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out ...)
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   intrinsic-class: IntrinsicMethodHandle (int,int)int
//   intrinsic-invokeExact: 7
//   intrinsic-invoke: 7
//   intrinsic-invokeWithArguments: 9
//   intrinsic-wrong-type: WrongMethodTypeException: handle's method type (int,int)int but found (int,int)long
//   intrinsic-inserted: 5
//   intrinsic-exact-invoker: 8
//   intrinsic-target-type: (int,int)int
//   accessor-class: IntrinsicMethodHandle (int[],int)int
//   accessor-invokeExact: 30
//   accessor-invoke: 20
//   accessor-oob: ArrayIndexOutOfBoundsException
//   accessor-cached: true
//
// Before wave 45 CratonVM's doors read their own slots out of the
// `IntrinsicMethodHandle`'s bounds (if the constructor completed at all): the
// `*-invoke*` rows answered `null` / `0` or an internal error. With
// `CRATONVM_DBG_MH_DISPATCH=1` a wave-45 build prints
// `[MH_DELEGATING] java/lang/invoke/MethodHandleImpl$IntrinsicMethodHandle (II)I -> target java/lang/invoke/MethodHandle`
// for each `intrinsic-*` invocation. If `intrinsic-class` prints an
// exception instead, the constructor (`DelegatingMethodHandle.chooseDelegatingForm`
// -> `makeReinvokerForm`, then `LambdaForm.prepare`) is the step-2 blocker
// the proposal names, and the row's text says which step.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L4W45DelegatingHandleTarget {
    static int max(int a, int b) {
        return Math.max(a, b);
    }

    static String shortType(MethodType mt) {
        StringBuilder sb = new StringBuilder("(");
        for (int i = 0; i < mt.parameterCount(); i++) {
            if (i > 0) sb.append(",");
            sb.append(mt.parameterType(i).getSimpleName());
        }
        return sb.append(")").append(mt.returnType().getSimpleName()).toString();
    }

    interface Row {
        String run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = r.run();
        } catch (Throwable t) {
            Throwable c = t;
            if (c instanceof java.lang.reflect.InvocationTargetException && c.getCause() != null) {
                c = c.getCause();
            }
            out = c.getClass().getSimpleName() + ": " + c.getMessage();
            if (c instanceof ArrayIndexOutOfBoundsException) {
                out = c.getClass().getSimpleName();
            }
        }
        System.out.println(name + ": " + out);
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static Object enumConstant(String className, String name) throws Exception {
        Class<?> c = Class.forName(className);
        return Enum.valueOf((Class) c, name);
    }

    static MethodHandle intrinsic;
    static MethodHandle accessor;

    public static void main(String[] args) throws Throwable {
        Class<?> impl = Class.forName("java.lang.invoke.MethodHandleImpl");
        Class<?> intrinsicEnum = Class.forName("java.lang.invoke.MethodHandleImpl$Intrinsic");
        Class<?> accessEnum = Class.forName("java.lang.invoke.MethodHandleImpl$ArrayAccess");
        Method makeIntrinsic = impl.getDeclaredMethod("makeIntrinsic", MethodHandle.class, intrinsicEnum);
        makeIntrinsic.setAccessible(true);
        Method makeAccessor = impl.getDeclaredMethod("makeArrayElementAccessor", Class.class, accessEnum);
        makeAccessor.setAccessible(true);

        MethodHandle max = MethodHandles.lookup().findStatic(L4W45DelegatingHandleTarget.class, "max",
                MethodType.methodType(int.class, int.class, int.class));

        row("intrinsic-class", () -> {
            intrinsic = (MethodHandle) makeIntrinsic.invoke(null, max,
                    enumConstant("java.lang.invoke.MethodHandleImpl$Intrinsic", "ARRAY_LOAD"));
            return intrinsic.getClass().getSimpleName() + " " + shortType(intrinsic.type());
        });
        row("intrinsic-invokeExact", () -> String.valueOf((int) intrinsic.invokeExact(3, 7)));
        row("intrinsic-invoke", () -> String.valueOf(intrinsic.invoke((Object) 7, (Object) 2)));
        row("intrinsic-invokeWithArguments", () -> String.valueOf(intrinsic.invokeWithArguments(9, 4)));
        row("intrinsic-wrong-type", () -> String.valueOf((long) intrinsic.invokeExact(3, 7)));
        row("intrinsic-inserted", () -> {
            MethodHandle ins = MethodHandles.insertArguments(intrinsic, 0, 5);
            return String.valueOf((int) ins.invokeExact(2));
        });
        row("intrinsic-exact-invoker", () -> {
            MethodHandle inv = MethodHandles.exactInvoker(intrinsic.type());
            return String.valueOf((int) inv.invokeExact(intrinsic, 8, 1));
        });
        row("intrinsic-target-type", () -> shortType(intrinsic.type()));

        row("accessor-class", () -> {
            accessor = (MethodHandle) makeAccessor.invoke(null, int[].class,
                    enumConstant("java.lang.invoke.MethodHandleImpl$ArrayAccess", "GET"));
            return accessor.getClass().getSimpleName() + " " + shortType(accessor.type());
        });
        int[] data = {10, 20, 30};
        row("accessor-invokeExact", () -> String.valueOf((int) accessor.invokeExact(data, 2)));
        row("accessor-invoke", () -> String.valueOf(accessor.invoke((Object) data, (Object) 1)));
        row("accessor-oob", () -> String.valueOf((int) accessor.invokeExact(data, 3)));
        row("accessor-cached", () -> {
            Object again = makeAccessor.invoke(null, int[].class,
                    enumConstant("java.lang.invoke.MethodHandleImpl$ArrayAccess", "GET"));
            return String.valueOf(again == accessor);
        });
    }
}
