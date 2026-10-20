// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Repro for the held InvocationHandler.invokeDefault row: findVirtual(..).type().changeReturnType(..)
// is a ClassCastException on the VM (Object[] ptypes), (String,String)Object on HotSpot.
import java.lang.invoke.*;
public class SbrW6TailMethodType {
    public interface G { default String dflt(String n) { return "d" + n; } }
    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle h = l.findVirtual(String.class, "concat", MethodType.methodType(String.class, String.class));
        MethodType t = h.type();
        System.out.println(t + " " + t.parameterArray().getClass() + " " + t.parameterList().getClass());
        try { System.out.println(t.changeReturnType(Object.class)); } catch (Throwable x) { System.out.println("virt " + x); }
        try {
            MethodHandle s = l.findSpecial(G.class, "dflt", MethodType.methodType(String.class, String.class), SbrW6TailMethodType.class);
            System.out.println("special " + s.type());
        } catch (Throwable x) { System.out.println("special-lookup " + x); }
        try {
            MethodHandle s = l.findStatic(SbrW6TailMethodType.class, "st", MethodType.methodType(String.class, String.class));
            System.out.println(s.type().changeReturnType(Object.class) + " " + s.type().parameterArray().getClass());
        } catch (Throwable x) { System.out.println("static " + x); }
    }
    static String st(String s) { return s; }
}
