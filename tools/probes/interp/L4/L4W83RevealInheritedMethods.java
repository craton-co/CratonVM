// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83 -- probe for
// docs/internal/fixed-bugs/i83-orch-reveal-direct-names-the-referring-class-of-an-inherited-static-FIXED-20261011.md.
// A METHOD found through a class or interface that inherits it resolves to its
// DECLARING class, as HotSpot's `init_method_MemberName` writes the resolved
// method's holder into the `MemberName`: `revealDirect` and
// `MethodHandleInfo.getDeclaringClass()` read it. An interface's `Object`
// method resolves to `Object` and reads `invokeVirtual`; a default method a
// class inherits stays the class's. Every handle is invoked too, so the moved
// holder still dispatches (virtually where it should), and the static row
// initializes only the declaring class.
//
// Run: javac -d out L4W83RevealInheritedMethods.java
//      cratonvm [--nojit] [--compatible | --jdk-only] -cp out L4W83RevealInheritedMethods
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// static: invokeStatic Base.s ()String -> s initialized=[Base]
// static-own: invokeStatic Sub.own ()String -> own
// virtual: invokeVirtual Sub.m ()String -> sub-m
// virtual-own: invokeVirtual Sub.m2 ()String -> m2
// special: invokeSpecial Base.inh ()String -> inh
// object: invokeVirtual Base.hashCode ()int -> 7
// iface-object: invokeVirtual Object.hashCode ()int -> 7
// iface-super: invokeInterface SuperI.si ()String -> si
// iface-own: invokeInterface SubI.sub ()String -> sub
// default: invokeVirtual Impl.d ()String -> d
// abstract: invokeVirtual Sub.a ()String -> sub-a
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W83RevealInheritedMethods {
    static final List<String> INITIALIZED = new ArrayList<>();

    public abstract static class Base {
        static {
            INITIALIZED.add("Base");
        }

        public static String s() {
            return "s";
        }

        public String m() {
            return "base-m";
        }

        public abstract String a();

        public String inh() {
            return "inh";
        }

        @Override
        public int hashCode() {
            return 7;
        }
    }

    public static class Sub extends Base {
        static {
            INITIALIZED.add("Sub");
        }

        public static String own() {
            return "own";
        }

        @Override
        public String m() {
            return "sub-m";
        }

        public String m2() {
            return "m2";
        }

        @Override
        public String a() {
            return "sub-a";
        }
    }

    public static class Leaf extends Sub {
    }

    public interface SuperI {
        String si();
    }

    public interface SubI extends SuperI {
        String sub();
    }

    public static class Both implements SubI {
        public String si() {
            return "si";
        }

        public String sub() {
            return "sub";
        }

        @Override
        public int hashCode() {
            return 7;
        }
    }

    public interface D {
        default String d() {
            return "d";
        }
    }

    public static class Impl implements D {
    }

    static String reveal(MethodHandles.Lookup lk, MethodHandle mh) {
        MethodHandleInfo info = lk.revealDirect(mh);
        return MethodHandleInfo.referenceKindToString(info.getReferenceKind()) + " "
                + info.getDeclaringClass().getSimpleName() + "." + info.getName() + " "
                + info.getMethodType();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodType str = MethodType.methodType(String.class);
        MethodType integer = MethodType.methodType(int.class);

        MethodHandle h = lk.findStatic(Leaf.class, "s", str);
        Object r = h.invoke();
        System.out.println("static: " + reveal(lk, h) + " -> " + r + " initialized=" + INITIALIZED);

        h = lk.findStatic(Sub.class, "own", str);
        System.out.println("static-own: " + reveal(lk, h) + " -> " + h.invoke());

        Sub sub = new Sub();
        h = lk.findVirtual(Sub.class, "m", str);
        System.out.println("virtual: " + reveal(lk, h) + " -> " + h.invoke(sub));

        h = lk.findVirtual(Sub.class, "m2", str);
        System.out.println("virtual-own: " + reveal(lk, h) + " -> " + h.invoke(sub));

        MethodHandles.Lookup leafLk = MethodHandles.privateLookupIn(Leaf.class, lk);
        h = leafLk.findSpecial(Sub.class, "inh", str, Leaf.class);
        System.out.println("special: " + reveal(leafLk, h) + " -> " + h.invoke(new Leaf()));

        h = lk.findVirtual(Sub.class, "hashCode", integer);
        System.out.println("object: " + reveal(lk, h) + " -> " + h.invoke(sub));

        h = lk.findVirtual(SubI.class, "hashCode", integer);
        System.out.println("iface-object: " + reveal(lk, h) + " -> " + h.invoke(new Both()));

        h = lk.findVirtual(SubI.class, "si", str);
        System.out.println("iface-super: " + reveal(lk, h) + " -> " + h.invoke(new Both()));

        h = lk.findVirtual(SubI.class, "sub", str);
        System.out.println("iface-own: " + reveal(lk, h) + " -> " + h.invoke(new Both()));

        h = lk.findVirtual(Impl.class, "d", str);
        System.out.println("default: " + reveal(lk, h) + " -> " + h.invoke(new Impl()));

        h = lk.findVirtual(Leaf.class, "a", str);
        System.out.println("abstract: " + reveal(lk, h) + " -> " + h.invoke(new Leaf()));
    }
}
