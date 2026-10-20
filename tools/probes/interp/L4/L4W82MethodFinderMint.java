// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L4: a repeat `Lookup` METHOD finder
// answers exactly what the first call answered, whether or not
//
//   CRATONVM_MH_METHOD_FINDER_MINT_MEMO=1  (--jdk-only; no JIT, or the
//       `method-finder-mint` row of CRATONVM_INTRINSIC_BODY_COMPILES_ROWS)
//       the five method finders (findVirtual, findStatic, findSpecial,
//       findConstructor, unreflect) are `Intrinsic`s that mint a repeat
//       call's handle and MemberName from the JDK body's first answer.
//
// Every finder below is called three times (the first fills the memo, the
// next two are minted); each answer is invoked (invokeExact, and for
// findVirtual also invoke / bindTo / asType), compared with the previous
// round's for freshness and cracked with revealDirect. Every species is
// covered: DirectMethodHandle (virtual, static, unreflect), $Interface,
// $Special (own class and a super call through a subclass), $Constructor,
// and a protected member's receiver restriction (Object.clone). The
// declines too: a caller-sensitive method (Class.forName), a varargs method,
// a missing method, the wrong reference kind. Access per caller and mode: a
// private method through a full lookup, the same lookup class without
// PRIVATE, and publicLookup (three keys, alternating); a findSpecial through
// publicLookup; an unreflect of a private Method with and without
// setAccessible (the `override` bit is a key word). Initialization: a
// static method of a class not yet initialized (the finder must not
// initialize it, the first invocation of a minted handle does, and a later
// finder call sees it initialized), and an inherited static method found
// through an uninitialized subclass (only the superclass initializes).
//
// Run (all must print the block below): --jdk-only --nojit with and without
// the switch, --jdk-only (JIT) with the switch and
// CRATONVM_INTRINSIC_BODY_COMPILES_ROWS=finder-mint,method-finder-mint, and
// --compatible (control), each with and without the switch.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// virtual#0 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int invoke=3 bound=3 astype=3
// virtual#1 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int invoke=3 bound=3 astype=3
// virtual#2 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int invoke=3 bound=3 astype=3
// interface#0 value=103 type=(Runner)int fresh=true reveal=invokeInterface Runner.run ()int
// interface#1 value=103 type=(Runner)int fresh=true reveal=invokeInterface Runner.run ()int
// interface#2 value=103 type=(Runner)int fresh=true reveal=invokeInterface Runner.run ()int
// static#0 value=5 type=(int)int fresh=true reveal=invokeStatic Box.st (int)int
// static#1 value=5 type=(int)int fresh=true reveal=invokeStatic Box.st (int)int
// static#2 value=5 type=(int)int fresh=true reveal=invokeStatic Box.st (int)int
// special-own#0 value=3 type=(Box)int fresh=true reveal=invokeSpecial Box.m ()int
// special-own#1 value=3 type=(Box)int fresh=true reveal=invokeSpecial Box.m ()int
// special-own#2 value=3 type=(Box)int fresh=true reveal=invokeSpecial Box.m ()int
// special-super#0 value=1 type=(Sub)int fresh=true reveal=invokeSpecial Base.m ()int
// special-public#0 java.lang.IllegalAccessException
// special-super#1 value=1 type=(Sub)int fresh=true reveal=invokeSpecial Base.m ()int
// special-public#1 java.lang.IllegalAccessException
// special-super#2 value=1 type=(Sub)int fresh=true reveal=invokeSpecial Base.m ()int
// special-public#2 java.lang.IllegalAccessException
// ctor#0 v=3 type=()Box fresh=true reveal=newInvokeSpecial Box.<init> ()void
// ctor#1 v=3 type=()Box fresh=true reveal=newInvokeSpecial Box.<init> ()void
// ctor#2 v=3 type=()Box fresh=true reveal=newInvokeSpecial Box.<init> ()void
// unreflect#0 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int
// unreflect#1 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int
// unreflect#2 value=3 type=(Box)int fresh=true reveal=invokeVirtual Box.m ()int
// unreflect-private#0 plain=java.lang.IllegalAccessException accessible=7
// unreflect-private#1 plain=java.lang.IllegalAccessException accessible=7
// unreflect-private#2 plain=java.lang.IllegalAccessException accessible=7
// protected-clone#0 type=(Box)Object copy=true v=3 fresh=true
// protected-clone#1 type=(Box)Object copy=true v=3 fresh=true
// protected-clone#2 type=(Box)Object copy=true v=3 fresh=true
// caller-sensitive#0 value=java.lang.String type=(String)Class
// caller-sensitive#1 value=java.lang.String type=(String)Class
// caller-sensitive#2 value=java.lang.String type=(String)Class
// varargs#0 value=2 varargs=true type=(String[])int
// varargs#1 value=2 varargs=true type=(String[])int
// varargs#2 value=2 varargs=true type=(String[])int
// private-full#0 value=7
// private-noprivate#0 java.lang.IllegalAccessException
// private-public#0 java.lang.IllegalAccessException
// private-full#1 value=7
// private-noprivate#1 java.lang.IllegalAccessException
// private-public#1 java.lang.IllegalAccessException
// private-full#2 value=7
// private-noprivate#2 java.lang.IllegalAccessException
// private-public#2 java.lang.IllegalAccessException
// missing#0 java.lang.NoSuchMethodException
// missing#1 java.lang.NoSuchMethodException
// missing#2 java.lang.NoSuchMethodException
// wrong-kind#0 java.lang.IllegalAccessException
// wrong-kind#1 java.lang.IllegalAccessException
// wrong-kind#2 java.lang.IllegalAccessException
// two-callers#0 a=3 b=3 distinct=true
// two-callers#1 a=3 b=3 distinct=true
// two-callers#2 a=3 b=3 distinct=true
// uninit found=2 initialized=false fresh=true
// U.<clinit>
// uninit value=9 initialized=true
// uninit again value=9 first=9 reveal=invokeStatic U.get ()int
// inherited-static#0 initialized=false reveal=invokeStatic P2.pst ()int
// P2.<clinit>
// inherited-static#1 value=11 initialized=true reveal=invokeStatic P2.pst ()int
// inherited-static#2 value=11 initialized=true reveal=invokeStatic P2.pst ()int
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

public class L4W82MethodFinderMint {
    public interface Runner {
        int run();
    }

    public static class Box implements Runner, Cloneable {
        public int v = 3;

        public Box() {
        }

        public int m() {
            return v;
        }

        public int run() {
            return v + 100;
        }

        private int hidden() {
            return 7;
        }

        public static int st(int x) {
            return x + 1;
        }

        public static int va(String... xs) {
            return xs.length;
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    public static class Base {
        public int m() {
            return 1;
        }
    }

    public static class Sub extends Base {
        @Override
        public int m() {
            return 2;
        }

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    public static class U {
        static {
            System.out.println("U.<clinit>");
            Seen.u = true;
        }

        public static int get() {
            return 9;
        }
    }

    public static class P2 {
        static {
            System.out.println("P2.<clinit>");
            Seen.p2 = true;
        }

        public static int pst() {
            return 11;
        }
    }

    public static class Q2 extends P2 {
        static {
            System.out.println("Q2.<clinit>");
        }
    }

    /** Initialization witnesses: reading these never initializes U or P2. */
    static final class Seen {
        static boolean u;
        static boolean p2;
    }

    static String reveal(MethodHandles.Lookup lk, MethodHandle mh) {
        MethodHandleInfo info = lk.revealDirect(mh);
        return MethodHandleInfo.referenceKindToString(info.getReferenceKind()) + " "
                + info.getDeclaringClass().getSimpleName() + "." + info.getName() + " "
                + info.getMethodType();
    }

    static String fail(Throwable t) {
        return t.getClass().getName();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandles.Lookup pub = MethodHandles.publicLookup();
        MethodHandles.Lookup boxLk = Box.lookup();
        MethodHandles.Lookup subLk = Sub.lookup();
        MethodType mtI = MethodType.methodType(int.class);
        Box box = new Box();

        MethodHandle prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findVirtual(Box.class, "m", mtI);
            int value = (int) h.invokeExact(box);
            int viaInvoke = (int) h.invoke(box);
            int bound = (int) h.bindTo(box).invoke();
            Object astype = h.asType(MethodType.methodType(Object.class, Object.class)).invokeExact((Object) box);
            System.out.println("virtual#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(lk, h)
                    + " invoke=" + viaInvoke + " bound=" + bound + " astype=" + astype);
            prev = h;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findVirtual(Runner.class, "run", mtI);
            int value = (int) h.invokeExact((Runner) box);
            System.out.println("interface#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(lk, h));
            prev = h;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findStatic(Box.class, "st", MethodType.methodType(int.class, int.class));
            int value = (int) h.invokeExact(4);
            System.out.println("static#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(lk, h));
            prev = h;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = boxLk.findSpecial(Box.class, "m", mtI, Box.class);
            int value = (int) h.invokeExact(box);
            System.out.println("special-own#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(boxLk, h));
            prev = h;
        }
        prev = null;
        Sub sub = new Sub();
        for (int i = 0; i < 3; i++) {
            MethodHandle h = subLk.findSpecial(Base.class, "m", mtI, Sub.class);
            int value = (int) h.invokeExact(sub);
            System.out.println("special-super#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(subLk, h));
            prev = h;
            try {
                pub.findSpecial(Base.class, "m", mtI, Sub.class);
                System.out.println("special-public#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("special-public#" + i + " " + fail(t));
            }
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findConstructor(Box.class, MethodType.methodType(void.class));
            Box made = (Box) h.invokeExact();
            System.out.println("ctor#" + i + " v=" + made.v + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(lk, h));
            prev = h;
        }
        prev = null;
        Method mMethod = Box.class.getMethod("m");
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.unreflect(mMethod);
            int value = (int) h.invokeExact(box);
            System.out.println("unreflect#" + i + " value=" + value + " type=" + h.type()
                    + " fresh=" + (h != prev) + " reveal=" + reveal(lk, h));
            prev = h;
        }
        Method hidden = Box.class.getDeclaredMethod("hidden");
        for (int i = 0; i < 3; i++) {
            String plain;
            try {
                pub.unreflect(hidden);
                plain = "no exception";
            } catch (Throwable t) {
                plain = fail(t);
            }
            hidden.setAccessible(true);
            MethodHandle h = pub.unreflect(hidden);
            int value = (int) h.invokeExact(box);
            hidden.setAccessible(false);
            System.out.println("unreflect-private#" + i + " plain=" + plain + " accessible=" + value);
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle h = boxLk.findVirtual(Box.class, "clone", MethodType.methodType(Object.class));
            Object copy = (Object) h.invokeExact(box);
            System.out.println("protected-clone#" + i + " type=" + h.type()
                    + " copy=" + (copy != box && copy instanceof Box) + " v=" + ((Box) copy).v
                    + " fresh=" + (h != prev));
            prev = h;
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findStatic(Class.class, "forName", MethodType.methodType(Class.class, String.class));
            Class<?> c = (Class<?>) h.invokeExact("java.lang.String");
            System.out.println("caller-sensitive#" + i + " value=" + c.getName() + " type=" + h.type());
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findStatic(Box.class, "va", MethodType.methodType(int.class, String[].class));
            int value = (int) h.invoke("a", "b");
            System.out.println("varargs#" + i + " value=" + value + " varargs=" + h.isVarargsCollector()
                    + " type=" + h.type());
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findVirtual(Box.class, "hidden", mtI);
            System.out.println("private-full#" + i + " value=" + (int) h.invokeExact(box));
            try {
                lk.dropLookupMode(MethodHandles.Lookup.PRIVATE).findVirtual(Box.class, "hidden", mtI);
                System.out.println("private-noprivate#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("private-noprivate#" + i + " " + fail(t));
            }
            try {
                pub.findVirtual(Box.class, "hidden", mtI);
                System.out.println("private-public#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("private-public#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            try {
                lk.findVirtual(Box.class, "nope", mtI);
                System.out.println("missing#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("missing#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            try {
                lk.findStatic(Box.class, "m", mtI);
                System.out.println("wrong-kind#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("wrong-kind#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle a = lk.findVirtual(Box.class, "m", mtI);
            MethodHandle b = boxLk.findVirtual(Box.class, "m", mtI);
            System.out.println("two-callers#" + i + " a=" + (int) a.invokeExact(box) + " b="
                    + (int) b.invokeExact(box) + " distinct=" + (a != b));
        }

        // A static method of a class not yet initialized: the finder does not
        // initialize it (the second call is a repeat), the minted handle's
        // first invocation does, and a later finder call sees it initialized.
        MethodHandle u1 = lk.findStatic(U.class, "get", mtI);
        MethodHandle u2 = lk.findStatic(U.class, "get", mtI);
        System.out.println("uninit found=" + ((u1 != null ? 1 : 0) + (u2 != null ? 1 : 0))
                + " initialized=" + Seen.u + " fresh=" + (u1 != u2));
        int uv = (int) u2.invokeExact();
        System.out.println("uninit value=" + uv + " initialized=" + Seen.u);
        MethodHandle u3 = lk.findStatic(U.class, "get", mtI);
        System.out.println("uninit again value=" + (int) u3.invokeExact() + " first="
                + (int) u1.invokeExact() + " reveal=" + reveal(lk, u3));

        // An inherited static method through an uninitialized subclass: only
        // the declaring class initializes, at the first invocation.
        for (int i = 0; i < 3; i++) {
            MethodHandle h = lk.findStatic(Q2.class, "pst", mtI);
            if (i == 0) {
                System.out.println("inherited-static#" + i + " initialized=" + Seen.p2
                        + " reveal=" + reveal(lk, h));
            } else {
                int value = (int) h.invokeExact();
                System.out.println("inherited-static#" + i + " value=" + value + " initialized="
                        + Seen.p2 + " reveal=" + reveal(lk, h));
            }
        }
    }
}
