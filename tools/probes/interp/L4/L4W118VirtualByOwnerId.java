// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 118, lane L4 (`CRATONVM_MH_VIRTUAL_BY_OWNER_ID`,
// default off; the `VIRTUAL` half of
// docs/internal/fixed-bugs/interpreter-i63-L4-proposal-virtual-door-dispatches-by-owner-id-KEPT-OFF-20261224.md):
// `findVirtual` handles over a class hierarchy, called with receivers of the
// declaring class and of subclasses that override or inherit, through
// invokeExact / invoke / asType / bindTo / invokeWithArguments, an interface
// member, an abstract member, Object's members and a null receiver. Every row
// must be HotSpot's with the switch off and on, both modes, JIT and --nojit
// (the switch only changes how the VM finds the declared class on the
// receiver's chain: by id, not by name).
//
// Run: javac -d . L4W118VirtualByOwnerId.java
//      java -cp . L4W118VirtualByOwnerId                                   (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W118VirtualByOwnerId
//      CRATONVM_MH_VIRTUAL_BY_OWNER_ID=1 [CRATONVM_MH_INSTANCE_OWNER_ID=1] cratonvm ... L4W118VirtualByOwnerId
//      Bench: L4W63OwnerIdBench / L4W62OwnerCastBench rows `virtual-invoke`, switch off vs on.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// exact base mid mid
// invoke base mid mid
// astype mid base
// bound mid mid
// iwa mid base
// abstract 3 0
// interface shape:3 shape:0
// object Sub! x
// warm 16667
// null java.lang.NullPointerException
// cce java.lang.ClassCastException
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;

public class L4W118VirtualByOwnerId {
    interface Named {
        String name();
    }

    abstract static class Shape implements Named {
        abstract int sides();

        public String who() {
            return "shape";
        }

        public String name() {
            return "shape:" + sides();
        }
    }

    static class Base extends Shape {
        int sides() {
            return 0;
        }

        public String who() {
            return "base";
        }
    }

    static class Mid extends Base {
        int sides() {
            return 3;
        }

        public String who() {
            return "mid";
        }
    }

    static class Sub extends Mid {
        // inherits who() and sides() from Mid
        public String toString() {
            return "Sub!";
        }
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType str = MethodType.methodType(String.class);
        MethodHandle who = l.findVirtual(Base.class, "who", str);
        MethodHandle sides = l.findVirtual(Shape.class, "sides", MethodType.methodType(int.class));
        MethodHandle name = l.findVirtual(Named.class, "name", str);
        MethodHandle ts = l.findVirtual(Object.class, "toString", str);
        Base b = new Base();
        Mid m = new Mid();
        Sub s = new Sub();

        System.out.println("exact " + (String) who.invokeExact(b) + " " + (String) who.invokeExact((Base) m)
                + " " + (String) who.invokeExact((Base) s));
        System.out.println("invoke " + who.invoke(b) + " " + who.invoke(m) + " " + who.invoke(s));
        MethodHandle whoObj = who.asType(MethodType.methodType(Object.class, Object.class));
        System.out.println("astype " + whoObj.invoke((Object) s) + " " + whoObj.invoke((Object) b));
        System.out.println("bound " + who.bindTo(s).invoke() + " " + who.bindTo(m).invoke());
        System.out.println("iwa " + who.invokeWithArguments(s) + " " + who.invokeWithArguments(List.of(b)));
        System.out.println("abstract " + (int) sides.invokeExact((Shape) s) + " " + (int) sides.invokeExact((Shape) b));
        System.out.println("interface " + (String) name.invokeExact((Named) s) + " " + (String) name.invokeExact((Named) b));
        System.out.println("object " + (String) ts.invokeExact((Object) s) + " "
                + ((String) ts.invokeExact((Object) "x")));
        int warm = 0;
        for (int i = 0; i < 5000; i++) {
            Base r = (i % 3 == 0) ? b : (i % 3 == 1) ? m : s;
            warm += ((String) who.invokeExact(r)).length();
        }
        System.out.println("warm " + warm);
        try {
            String r = (String) who.invokeExact((Base) null);
            System.out.println("null none " + r);
        } catch (NullPointerException e) {
            System.out.println("null " + e.getClass().getName());
        }
        try {
            Object r = whoObj.invoke((Object) "notabase");
            System.out.println("cce none " + r);
        } catch (ClassCastException e) {
            System.out.println("cce " + e.getClass().getName());
        }
    }
}
