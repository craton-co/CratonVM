// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 105, lane L4: `MethodHandleNatives.resolve` wrote
// the modifiers of a DIFFERENT overload into the `MemberName` of a method the
// named class inherits. `declared_method_access_flags` (superclass walk) took
// "the one method of that name" a class declares when no descriptor matched
// (a rule meant for signature-polymorphic members), so the subclass's own
// overload won over the inherited exact match:
//
//   findVirtual(Sub, "foo", ()V)  Sub declares only `static void foo(int)`:
//                                 the member read ACC_STATIC and
//                                 `Lookup.checkMethod` refused it ("expected
//                                 a non-static method"); HotSpot finds Base.foo.
//   findVirtual(Sub, "bar", ()I)  Sub declares only `private int bar(String)`:
//                                 the inherited public method read private.
//   findStatic(Sub, "baz", ()I)   Sub declares only an instance `baz(long)`:
//                                 "expected a static method".
//
// Both modes (the native runs in every boot). The top-level classes below are
// not nestmates of the probe class, so a private flag would be refused.
// HotSpot resolves the exact descriptor (JVMS 5.4.3.3) and
// `init_method_MemberName` writes that method's own flags.
//
// Run: javac -d . L4W105ResolveOverloadFlags.java
//      java -cp . L4W105ResolveOverloadFlags                (HotSpot)
//      cratonvm --jdk-only -cp . L4W105ResolveOverloadFlags
//      cratonvm --compatible -cp . L4W105ResolveOverloadFlags
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// virtual-over-static-overload 11 mods=1 static=false
// virtual-over-private-overload 1 mods=1
// static-over-instance-overload 3 mods=9 static=true
// exact-own-overload 22 mods=9
// control-own-private ok java.lang.IllegalAccessException
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;

class L4W105RoBase {
    public int foo() {
        return 11;
    }

    public int bar() {
        return 1;
    }

    public static int baz() {
        return 3;
    }
}

class L4W105RoSub extends L4W105RoBase {
    public static int foo(int x) {
        return 21;
    }

    private int bar(String s) {
        return 2;
    }

    public long baz(long x) {
        return 4;
    }

    public static int own(int x) {
        return 22;
    }

    private int hidden() {
        return 5;
    }
}

public class L4W105ResolveOverloadFlags {
    static String describe(Throwable t) {
        return t.getClass().getName();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        L4W105RoSub sub = new L4W105RoSub();

        try {
            MethodHandle mh = lookup.findVirtual(L4W105RoSub.class, "foo", MethodType.methodType(int.class));
            MethodHandleInfo info = lookup.revealDirect(mh);
            System.out.println("virtual-over-static-overload " + (int) mh.invoke(sub)
                    + " mods=" + info.getModifiers()
                    + " static=" + Modifier.isStatic(info.getModifiers()));
        } catch (Throwable t) {
            System.out.println("virtual-over-static-overload " + describe(t) + ": " + t.getMessage());
        }

        try {
            MethodHandle mh = lookup.findVirtual(L4W105RoSub.class, "bar", MethodType.methodType(int.class));
            MethodHandleInfo info = lookup.revealDirect(mh);
            System.out.println("virtual-over-private-overload " + (int) mh.invoke(sub)
                    + " mods=" + info.getModifiers());
        } catch (Throwable t) {
            System.out.println("virtual-over-private-overload " + describe(t) + ": " + t.getMessage());
        }

        try {
            MethodHandle mh = lookup.findStatic(L4W105RoSub.class, "baz", MethodType.methodType(int.class));
            MethodHandleInfo info = lookup.revealDirect(mh);
            System.out.println("static-over-instance-overload " + (int) mh.invoke()
                    + " mods=" + info.getModifiers()
                    + " static=" + Modifier.isStatic(info.getModifiers()));
        } catch (Throwable t) {
            System.out.println("static-over-instance-overload " + describe(t) + ": " + t.getMessage());
        }

        // CONTROL: the class's own exact overload is unchanged.
        try {
            MethodHandle mh = lookup.findStatic(L4W105RoSub.class, "own",
                    MethodType.methodType(int.class, int.class));
            MethodHandleInfo info = lookup.revealDirect(mh);
            System.out.println("exact-own-overload " + (int) mh.invoke(0) + " mods=" + info.getModifiers());
        } catch (Throwable t) {
            System.out.println("exact-own-overload " + describe(t) + ": " + t.getMessage());
        }

        // CONTROL: a private method of a non-nestmate is still refused.
        try {
            lookup.findVirtual(L4W105RoSub.class, "hidden", MethodType.methodType(int.class));
            System.out.println("control-own-private found");
        } catch (Throwable t) {
            System.out.println("control-own-private ok " + describe(t));
        }
    }
}
