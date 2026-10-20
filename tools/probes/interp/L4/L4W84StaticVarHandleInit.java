// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 84, lane L4: when `findStaticVarHandle` /
// `findStaticGetter` initialize the field's class. JDK 25:
// `VarHandles.makeFieldHandle` wraps a static field handle of a class that
// `shouldBeInitialized` in a `LazyInitializingVarHandle`, so creating the
// handle, `varType()`, `coordinateTypes()` and `toMethodHandle` run no
// `<clinit>`; the first access does (once). A static getter
// `DirectMethodHandle` initializes at its first invocation
// (`LF_INVSTATIC_INIT` / `ensureInitialized`). An inherited static field
// initializes its DECLARING class only (JVMS 5.5), here the interface `I`
// (which has a non-constant field, hence a `<clinit>`) and not `Sub`.
//
// Run: javac -d . L4W84StaticVarHandleInit.java
//      cratonvm --jdk-only [--nojit] -cp . L4W84StaticVarHandleInit
//      cratonvm --compatible [--nojit] -cp . L4W84StaticVarHandleInit
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// vh: found
// vh: varType=int coords=[]
// vh: toMethodHandle
// A.<clinit>
// vh: get=5
// vh: set-get=6
// getter: found
// B.<clinit>
// getter: get=7
// inherited: found
// I.<clinit>
// inherited: get=8
// sub-initialized=false
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W84StaticVarHandleInit {
    static boolean subInitialized;

    public static class A {
        static {
            System.out.println("A.<clinit>");
        }

        public static int X = 5;
    }

    public static class B {
        static {
            System.out.println("B.<clinit>");
        }

        public static int Y = 7;
    }

    public interface I {
        Object SIDE = side();
        int Z = Integer.parseInt("8");

        private static Object side() {
            System.out.println("I.<clinit>");
            return "s";
        }
    }

    public static class Sub implements I {
        static {
            subInitialized = true;
            System.out.println("Sub.<clinit>");
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle vh = l.findStaticVarHandle(A.class, "X", int.class);
        System.out.println("vh: found");
        System.out.println("vh: varType=" + vh.varType() + " coords=" + vh.coordinateTypes());
        vh.toMethodHandle(VarHandle.AccessMode.GET);
        System.out.println("vh: toMethodHandle");
        int x = (int) vh.get();
        System.out.println("vh: get=" + x);
        vh.set(6);
        System.out.println("vh: set-get=" + (int) vh.get());

        MethodHandle getter = l.findStaticGetter(B.class, "Y", int.class);
        System.out.println("getter: found");
        int y = (int) getter.invokeExact();
        System.out.println("getter: get=" + y);

        VarHandle inherited = l.findStaticVarHandle(Sub.class, "Z", int.class);
        System.out.println("inherited: found");
        int z = (int) inherited.get();
        System.out.println("inherited: get=" + z);
        System.out.println("sub-initialized=" + subInitialized);
    }
}
