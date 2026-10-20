// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 124, lane L4 (page
// docs/internal/proposals/interpreter/i88-L3-proposal-first-use-reflective-constructor-cost-20261121.md):
// the `MethodType` a constructor's `MemberName` carries when it was made from a reflected
// `Constructor` (`Lookup.unreflectConstructor`, and the reflective `newInstance` accessor).
//
// HotSpot's `MethodHandleNatives.init` leaves `MemberName.type` unset for a constructor; the JDK's
// `MemberName(Constructor)` stores `{void.class, ptypes}` and `getMethodType()` interns it.
// CratonVM's native minted a fresh, non-interned `(ptypes)void` whose private `MethodTypeForm`
// named the type itself as its basic and erased type. Visible through
// `revealDirect(...).getMethodType()`: its `erase()` kept reference parameters, and `==` against
// `MethodType.methodType(..)` was false.
//
// Switch: CRATONVM_MHN_INIT_CONSTRUCTOR_JDK_TYPE (default ON; `=0` restores the minted type, under
// which the `interned` rows, the `erase` rows with a reference parameter and the `hasPrimitives`
// rows (the minted form counted only parameters; `void` is a primitive) are
// expected to DIFF).
//
// Run: javac -d out L4W124CtorMemberNameType.java
//      cratonvm [--compatible] [--nojit] -cp out L4W124CtorMemberNameType
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// unreflect (String,int): type (String,int)void
// unreflect (String,int): interned true
// unreflect (String,int): erase (Object,int)void
// unreflect (String,int): generic (Object,Object)Object
// unreflect (String,int): wrap (String,Integer)Void
// unreflect (String,int): hasPrimitives true hasWrappers false
// unreflect (String,int): exact-invoker (MethodHandle,String,int)void made 1
// unreflect (): type ()void
// unreflect (): interned true
// unreflect (): erase ()void
// unreflect (): generic ()Object
// unreflect (): wrap ()Void
// unreflect (): hasPrimitives true hasWrappers false
// unreflect (): exact-invoker (MethodHandle)void made 2
// unreflect (Object[]): type (Object[])void
// unreflect (Object[]): interned true
// unreflect (Object[]): erase (Object)void
// unreflect (Object[]): generic (Object)Object
// unreflect (Object[]): wrap (Object[])Void
// unreflect (Object[]): hasPrimitives true hasWrappers false
// unreflect (Object[]): exact-invoker (MethodHandle,Object[])void made 3
// newInstance: Box(s,7) Box() Box(1)
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;

public class L4W124CtorMemberNameType {
    static int made;

    public static final class Box {
        final String text;

        public Box(String s, int i) {
            made++;
            text = "Box(" + s + "," + i + ")";
        }

        public Box() {
            made++;
            text = "Box()";
        }

        public Box(Object[] a) {
            made++;
            text = "Box(" + a.length + ")";
        }
    }

    static void row(String label, Constructor<?> c, MethodType expected, Object... args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle h = lookup.unreflectConstructor(c);
        MethodHandleInfo info = lookup.revealDirect(h);
        MethodType t = info.getMethodType();
        String p = "unreflect " + label + ": ";
        System.out.println(p + "type " + t);
        System.out.println(p + "interned " + (t == expected));
        System.out.println(p + "erase " + t.erase());
        System.out.println(p + "generic " + t.generic());
        System.out.println(p + "wrap " + t.wrap());
        System.out.println(p + "hasPrimitives " + t.hasPrimitives() + " hasWrappers " + t.hasWrappers());
        MethodHandle inv = MethodHandles.exactInvoker(t);
        MethodHandle dropped = MethodHandles.dropReturn(h);
        Object[] all = new Object[args.length + 1];
        all[0] = dropped;
        System.arraycopy(args, 0, all, 1, args.length);
        inv.invokeWithArguments(all);
        System.out.println(p + "exact-invoker " + inv.type() + " made " + made);
    }

    public static void main(String[] args) throws Throwable {
        row("(String,int)", Box.class.getConstructor(String.class, int.class),
                MethodType.methodType(void.class, String.class, int.class), "s", 7);
        row("()", Box.class.getConstructor(), MethodType.methodType(void.class));
        row("(Object[])", Box.class.getConstructor(Object[].class),
                MethodType.methodType(void.class, Object[].class), (Object) new Object[] {"x"});
        Box a = Box.class.getConstructor(String.class, int.class).newInstance("s", 7);
        Box b = Box.class.getConstructor().newInstance();
        Box c = Box.class.getConstructor(Object[].class).newInstance((Object) new Object[] {"y"});
        System.out.println("newInstance: " + a.text + " " + b.text + " " + c.text);
    }
}
