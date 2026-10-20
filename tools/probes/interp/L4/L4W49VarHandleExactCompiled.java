// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L4 (pages
// `interpreter-L4-varhandle-invoke-exact-behavior-is-not-enforced-FIXED-20261013` and
// `interpreter-L4-a-compiled-varhandle-site-skips-the-invoke-exact-check-FIXED-20261013`): an
// invoke-exact `VarHandle` reaching a WARM (compiled) call site whose
// symbolic type is not the access mode's exact type. With the JIT on, a
// `VarHandle.get` / `set` / `compareAndSet` site over one reference
// coordinate is baked into a thin helper whose cold arm had no call-site
// descriptor, so an exact handle's access was converted as a plain one.
// Each helper is warmed 20,000 times with the PLAIN handle, then handed the
// exact one. The array rows use a JDK-minted exact handle
// (`arrayElementVarHandle(..).withInvokeExactBehavior()` is JDK bytecode),
// and `first-array` runs before any other exact access of the VM.
//
// Run: javac -d out L4W49VarHandleExactCompiled.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W49VarHandleExactCompiled
//
// Positive control: `CRATONVM_DBG_MH_STACK=1` prints an
// `[MH_STACK] exact VarHandle stand-in site -> ...` line for each thin-helper
// row with the JIT on.
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   first-array: WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object
//   get-long: WrongMethodTypeException: handle's method type (H)int but found (H)long after 20000
//   get-object-coordinate: WrongMethodTypeException: handle's method type (H)int but found (Object)int after 20000
//   get-exact-site: 200000 after 20000
//   set-object-coordinate: WrongMethodTypeException: handle's method type (H,int)void but found (Object,int)void after 20000
//   cas-object-coordinate: WrongMethodTypeException: handle's method type (H,int,int)boolean but found (Object,int,int)boolean after 20000
//   array-get-object: WrongMethodTypeException: handle's method type (int[],int)int but found (int[],int)Object after 20000
//   get-long-again: WrongMethodTypeException: handle's method type (H)int but found (H)long after 20000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W49VarHandleExactCompiled {
    static final int N = 20_000;

    static final class H {
        int i = 10;
    }

    static final VarHandle I;

    static {
        try {
            I = MethodHandles.lookup().findVarHandle(H.class, "i", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long getLong(VarHandle vh, H h) {
        return (long) vh.get(h);
    }

    static int getObjectCoordinate(VarHandle vh, Object h) {
        return (int) vh.get(h);
    }

    static int getExact(VarHandle vh, H h) {
        return (int) vh.get(h);
    }

    static void setObjectCoordinate(VarHandle vh, Object h, int v) {
        vh.set(h, v);
    }

    static boolean casObjectCoordinate(VarHandle vh, Object h, int a, int b) {
        return vh.compareAndSet(h, a, b);
    }

    static Object arrayGet(VarHandle vh, int[] a, int i) {
        return vh.get(a, i);
    }

    interface Warm {
        void call(VarHandle vh) throws Throwable;
    }

    static void row(String name, VarHandle plain, VarHandle exact, Warm body) {
        int done = 0;
        String out;
        try {
            for (int k = 0; k < N; k++) {
                body.call(plain);
                done++;
            }
            body.call(exact);
            out = "no exception";
        } catch (Throwable t) {
            out = t.getClass().getSimpleName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out + " after " + done);
    }

    public static void main(String[] args) {
        VarHandle plainArray = MethodHandles.arrayElementVarHandle(int[].class);
        VarHandle exactArray = plainArray.withInvokeExactBehavior();
        int[] arr = {7};
        try {
            Object o = exactArray.get(arr, 0);
            System.out.println("first-array: no exception " + o);
        } catch (Throwable t) {
            System.out.println("first-array: " + t.getClass().getSimpleName() + ": " + t.getMessage());
        }
        VarHandle exact = I.withInvokeExactBehavior();
        H h = new H();
        row("get-long", I, exact, vh -> getLong(vh, h));
        row("get-object-coordinate", I, exact, vh -> getObjectCoordinate(vh, h));
        long[] sum = {0};
        int done = 0;
        String out;
        try {
            for (int k = 0; k < N; k++) {
                sum[0] += getExact(exact, h);
                done++;
            }
            out = String.valueOf(sum[0]);
        } catch (Throwable t) {
            out = t.getClass().getSimpleName() + ": " + t.getMessage();
        }
        System.out.println("get-exact-site: " + out + " after " + done);
        row("set-object-coordinate", I, exact, vh -> setObjectCoordinate(vh, h, 10));
        row("cas-object-coordinate", I, exact, vh -> casObjectCoordinate(vh, h, 10, 10));
        row("array-get-object", plainArray, exactArray, vh -> arrayGet(vh, arr, 0));
        row("get-long-again", I, exact, vh -> getLong(vh, h));
    }
}
