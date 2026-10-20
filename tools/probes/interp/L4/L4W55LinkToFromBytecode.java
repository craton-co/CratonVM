// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L4: a REAL JDK `DirectMethodHandle` whose
// `LambdaForm` linker the wave-54 member dispatch does not cover, so the
// linker's own bytecode runs and ends in `invokestatic MethodHandle.linkTo*`
// with the call's CONCRETE descriptor (`(Ljava/lang/Object;ILjava/lang/invoke/MemberName;)I`
// and the like): `DirectMethodHandle$Interface` (its linker checks the
// receiver first, `linkToInterface`), `$Constructor` (allocates first,
// `linkToSpecial` on the new object), and `$Special` over an interface caller
// (`invokeSpecialIFC`, a receiver check then `linkToSpecial`). Page
// `docs/known-issues/interpreter/i54-L4-linkto-intrinsics-are-not-reached-from-bytecode-20261018.md`.
// CratonVM before wave 55 (code read): `linkTo*` was registered under the
// erased `([Ljava/lang/Object;)Ljava/lang/Object;` only and its name was not
// in the signature-polymorphic re-probe, so the linker's call found no method
// (`NoSuchMethodError` / `InternalError`). Positive control:
// `CRATONVM_DBG_MH_DISPATCH=1` prints `[MH_LINKTO] linkToInterface<site>` /
// `linkToSpecial<site>` from bytecode; `CRATONVM_MH_LINKTO_FROM_BYTECODE=0`
// restores the miss.
//
// Needs JVM flags: --add-opens java.base/java.lang.invoke=ALL-UNNAMED
// (it builds the real handles reflectively through `DirectMethodHandle.make`).
//
// Run: javac -d out L4W55LinkToFromBytecode.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] \
//          --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W55LinkToFromBytecode
//   (HotSpot: java --add-opens java.base/java.lang.invoke=ALL-UNNAMED -cp out L4W55LinkToFromBytecode)
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   iface-class: java.lang.invoke.DirectMethodHandle$Interface (Shape)int
//   iface-exact: 9
//   iface-null: java.lang.NullPointerException: Cannot invoke L4W55LinkToFromBytecode$Shape.area()int/invokeInterface with null receiver
//   iface-iwa: 16
//   ctor-class: java.lang.invoke.DirectMethodHandle$Constructor (int)Pt
//   ctor-exact: Pt(5)
//   ctor-iwa: Pt(6)
//   special-ifc-class: java.lang.invoke.DirectMethodHandle$Special (Shape)String
//   special-ifc-exact: Shape
//   special-ifc-virtual: Sq
//   special-ifc-null: java.lang.NullPointerException: Cannot invoke L4W55LinkToFromBytecode$Shape.describe()String/invokeSpecial with null receiver
//   iface-exact-again: 9
//   ctor-exact-again: Pt(5)
//   special-ifc-exact-again: Shape
import java.lang.invoke.MethodHandle;
import java.lang.reflect.Method;

public class L4W55LinkToFromBytecode {
    interface Shape {
        int area();

        default String describe() {
            return "Shape";
        }
    }

    static final class Sq implements Shape {
        final int side;

        Sq(int side) {
            this.side = side;
        }

        public int area() {
            return side * side;
        }

        public String describe() {
            return "Sq";
        }
    }

    static final class Pt {
        final int x;

        Pt(int x) {
            this.x = x;
        }

        public String toString() {
            return "Pt(" + x + ")";
        }
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

    static String shortType(MethodHandle h) {
        return h.type().toString();
    }

    static Object memberName(Object reflected) throws Throwable {
        Class<?> mn = Class.forName("java.lang.invoke.MemberName");
        java.lang.reflect.Constructor<?> ctor = reflected instanceof Method
                ? mn.getDeclaredConstructor(Method.class)
                : mn.getDeclaredConstructor(java.lang.reflect.Constructor.class);
        ctor.setAccessible(true);
        return ctor.newInstance(reflected);
    }

    /** `DirectMethodHandle.make(MemberName)`: `$Interface`, `$Constructor`, ... */
    static MethodHandle realDirect(Object reflected) throws Throwable {
        Class<?> mn = Class.forName("java.lang.invoke.MemberName");
        Method make = Class.forName("java.lang.invoke.DirectMethodHandle").getDeclaredMethod("make", mn);
        make.setAccessible(true);
        return (MethodHandle) make.invoke(null, memberName(reflected));
    }

    /** `DirectMethodHandle.make(REF_invokeSpecial, refc, member, caller)`. */
    static MethodHandle realSpecial(Method m, Class<?> caller) throws Throwable {
        Class<?> mn = Class.forName("java.lang.invoke.MemberName");
        Method make = Class.forName("java.lang.invoke.DirectMethodHandle")
                .getDeclaredMethod("make", byte.class, Class.class, mn, Class.class);
        make.setAccessible(true);
        return (MethodHandle) make.invoke(null, (byte) 7, m.getDeclaringClass(), memberName(m), caller);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle iface = realDirect(Shape.class.getMethod("area"));
        MethodHandle ctor = realDirect(Pt.class.getDeclaredConstructor(int.class));
        MethodHandle special = realSpecial(Shape.class.getMethod("describe"), Shape.class);

        row("iface-class", () -> iface.getClass().getName() + " " + shortType(iface));
        row("iface-exact", () -> (int) iface.invokeExact((Shape) new Sq(3)));
        row("iface-null", () -> (int) iface.invokeExact((Shape) null));
        row("iface-iwa", () -> iface.invokeWithArguments(new Sq(4)));
        row("ctor-class", () -> ctor.getClass().getName() + " " + shortType(ctor));
        row("ctor-exact", () -> (Pt) ctor.invokeExact(5));
        row("ctor-iwa", () -> ctor.invokeWithArguments(6));
        row("special-ifc-class", () -> special.getClass().getName() + " " + shortType(special));
        // invokespecial semantics: the interface's own default body, not Sq's
        // override.
        row("special-ifc-exact", () -> (String) special.invokeExact((Shape) new Sq(2)));
        row("special-ifc-virtual", () -> new Sq(2).describe());
        row("special-ifc-null", () -> (String) special.invokeExact((Shape) null));
        row("iface-exact-again", () -> (int) iface.invokeExact((Shape) new Sq(3)));
        row("ctor-exact-again", () -> (Pt) ctor.invokeExact(5));
        row("special-ifc-exact-again", () -> (String) special.invokeExact((Shape) new Sq(2)));
    }
}
