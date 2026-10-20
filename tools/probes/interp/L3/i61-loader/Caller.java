// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
public class Caller {
    public static int echo(Box box) { return box.value; }
    public static String run(MethodHandle foreign, MethodHandle foreignReturn) throws Throwable {
        MethodHandle own = MethodHandles.lookup().findStatic(Caller.class, "echo", MethodType.methodType(int.class, Box.class));
        int exact = (int) own.invokeExact(new Box(7));
        long converting = (long) own.invoke(new Box(8));
        String exactRefusal = "returned";
        try { int bad = (int) foreign.invokeExact(new Box(9)); }
        catch (Throwable error) { exactRefusal = error.getClass().getName(); }
        String convertingRefusal = "returned";
        try { int bad = (int) foreign.invoke(new Box(10)); }
        catch (Throwable error) { convertingRefusal = error.getClass().getName(); }
        String returnRefusal = "returned";
        try { Box bad = (Box) foreignReturn.invoke(); }
        catch (Throwable error) { returnRefusal = error.getClass().getName(); }
        int after = (int) own.invokeExact(new Box(11));
        return "exact="+exact+" converting="+converting+" wrong-exact="+exactRefusal+" wrong-inexact="+convertingRefusal+" wrong-return="+returnRefusal+" after="+after;
    }
}
