// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 25, lane L4: `Lookup.revealDirect`, which
// CratonVM answers with its own native in both modes
// (`native-builtins/src/lang_invoke.rs`, `lookup_reveal_direct`), against
// HotSpot: the reference kind (an interface's `findVirtual` is
// `invokeInterface`), the member's real modifiers (`getModifiers`,
// `isVarArgs`), and the refusal of a handle that is not direct.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W25RevealDirect
//
// HotSpot 25 (25.0.3) prints:
//   virtual: invokeVirtual L4W25RevealDirect$Sq area ()int mods=public varargs=false | invokeVirtual L4W25RevealDirect$Sq.area:()int
//   interface: invokeInterface L4W25RevealDirect$Shape area ()int mods=public abstract varargs=false | invokeInterface L4W25RevealDirect$Shape.area:()int
//   interface via class: invokeVirtual L4W25RevealDirect$Sq label ()String mods=public varargs=false | invokeVirtual L4W25RevealDirect$Sq.label:()String
//   default: invokeInterface L4W25RevealDirect$Shape label ()String mods=public varargs=false | invokeInterface L4W25RevealDirect$Shape.label:()String
//   interface static: invokeStatic L4W25RevealDirect$Shape unit ()Shape mods=public static varargs=false | invokeStatic L4W25RevealDirect$Shape.unit:()Shape
//   private: invokeVirtual L4W25RevealDirect$Sq secret ()int mods=private varargs=false | invokeVirtual L4W25RevealDirect$Sq.secret:()int
//   special: invokeSpecial L4W25RevealDirect$Sq secret ()int mods=private varargs=false | invokeSpecial L4W25RevealDirect$Sq.secret:()int
//   protected final sync: invokeVirtual L4W25RevealDirect$Sq guarded ()int mods=protected final synchronized varargs=false | invokeVirtual L4W25RevealDirect$Sq.guarded:()int
//   native static: invokeStatic L4W25RevealDirect$Sq nat ()void mods=static native varargs=false | invokeStatic L4W25RevealDirect$Sq.nat:()void
//   varargs: invokeVirtual L4W25RevealDirect$Sq varargs (String[])void mods=transient varargs=true | invokeVirtual L4W25RevealDirect$Sq.varargs:(String[])void
//   constructor: newInvokeSpecial L4W25RevealDirect$Sq <init> ()void mods= varargs=false | newInvokeSpecial L4W25RevealDirect$Sq.<init>:()void
//   getter: getField L4W25RevealDirect$Sq side ()int mods= varargs=false | getField L4W25RevealDirect$Sq.side:()int
//   setter: putField L4W25RevealDirect$Sq side (int)void mods= varargs=false | putField L4W25RevealDirect$Sq.side:(int)void
//   static getter: getStatic L4W25RevealDirect$Sq count ()int mods=static varargs=false | getStatic L4W25RevealDirect$Sq.count:()int
//   static setter: putStatic L4W25RevealDirect$Sq count (int)void mods=static varargs=false | putStatic L4W25RevealDirect$Sq.count:(int)void
//   jdk interface: invokeInterface java.util.List size ()int mods=public abstract varargs=false | invokeInterface java.util.List.size:()int
//   jdk static: invokeStatic java.lang.Integer parseInt (String)int mods=public static varargs=false | invokeStatic java.lang.Integer.parseInt:(String)int
//   bound: java.lang.IllegalArgumentException: not a direct method handle
//   adapted: java.lang.IllegalArgumentException: not a direct method handle
//
// Before wave 25 (read from the code, not run): every `mods=` was `public`
// (plus `static` for a static member) and every `varargs=` `false`; the two
// interface rows and `jdk interface` said `invokeVirtual`; the four field rows
// printed `class java.lang.Integer` as the method type (measured on the merged
// wave-25 build; fixed in its follow-up); `bound` answered
// the member underneath. Still expected to differ after wave 25: `adapted`
// (CratonVM's `asType` stamps a copy of the direct handle, which carries the
// direct handle's slots -- the recorded `MhIdentityProbe` I13/I14 deviation).

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Modifier;
import java.util.List;

public class L4W25RevealDirect {
    interface Shape {
        int area();

        default String label() {
            return "shape";
        }

        static Shape unit() {
            return () -> 1;
        }
    }

    static class Sq implements Shape {
        int side = 2;
        static int count;

        public int area() {
            return side * side;
        }

        private int secret() {
            return 7;
        }

        protected final synchronized int guarded() {
            return 3;
        }

        static native void nat();

        void varargs(String... xs) {
        }
    }

    interface Call { Object run() throws Throwable; }

    static void row(String name, Call c) {
        String out;
        try {
            out = String.valueOf(c.run());
        } catch (Throwable t) {
            out = t.toString();
        }
        System.out.println(name + ": " + out);
    }

    static String show(MethodHandles.Lookup l, MethodHandle mh) {
        MethodHandleInfo i = l.revealDirect(mh);
        return MethodHandleInfo.referenceKindToString(i.getReferenceKind())
            + " " + i.getDeclaringClass().getName()
            + " " + i.getName()
            + " " + i.getMethodType()
            + " mods=" + Modifier.toString(i.getModifiers())
            + " varargs=" + i.isVarArgs()
            + " | " + i;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType mi = MethodType.methodType(int.class);
        row("virtual", () -> show(l, l.findVirtual(Sq.class, "area", mi)));
        row("interface", () -> show(l, l.findVirtual(Shape.class, "area", mi)));
        row("interface via class", () -> show(l, l.findVirtual(Sq.class, "label", MethodType.methodType(String.class))));
        row("default", () -> show(l, l.findVirtual(Shape.class, "label", MethodType.methodType(String.class))));
        row("interface static", () -> show(l, l.findStatic(Shape.class, "unit", MethodType.methodType(Shape.class))));
        row("private", () -> show(l, l.findVirtual(Sq.class, "secret", mi)));
        row("special", () -> {
            MethodHandles.Lookup p = MethodHandles.privateLookupIn(Sq.class, l);
            return show(p, p.findSpecial(Sq.class, "secret", mi, Sq.class));
        });
        row("protected final sync", () -> show(l, l.findVirtual(Sq.class, "guarded", mi)));
        row("native static", () -> show(l, l.findStatic(Sq.class, "nat", MethodType.methodType(void.class))));
        row("varargs", () -> show(l, l.findVirtual(Sq.class, "varargs", MethodType.methodType(void.class, String[].class))));
        row("constructor", () -> show(l, l.findConstructor(Sq.class, MethodType.methodType(void.class))));
        row("getter", () -> show(l, l.findGetter(Sq.class, "side", int.class)));
        row("setter", () -> show(l, l.findSetter(Sq.class, "side", int.class)));
        row("static getter", () -> show(l, l.findStaticGetter(Sq.class, "count", int.class)));
        row("static setter", () -> show(l, l.findStaticSetter(Sq.class, "count", int.class)));
        row("jdk interface", () -> show(l, l.findVirtual(List.class, "size", mi)));
        row("jdk static", () -> show(l, l.findStatic(Integer.class, "parseInt", MethodType.methodType(int.class, String.class))));
        row("bound", () -> show(l, l.findVirtual(Sq.class, "area", mi).bindTo(new Sq())));
        row("adapted", () -> show(l, l.findVirtual(Sq.class, "area", mi).asType(MethodType.methodType(Object.class, Sq.class))));
    }
}
