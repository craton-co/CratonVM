// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L4: a repeat `Lookup` field finder
// answers exactly what the first call answered, whether or not
//
//   CRATONVM_MH_FINDER_MINT_MEMO=1  (--jdk-only, no JIT) the four field
//       finders are `Intrinsic`s that mint a repeat call's handle and
//       MemberName from the JDK body's first answer.
//
// Every finder below is called three times (the first fills the memo, the
// next two are minted); each answer is used, compared for freshness and
// cracked with revealDirect. The declines are covered too: a final field's
// setter, a missing field, a wrong type, a private field through
// publicLookup (another key), an inherited static field (always the body),
// and a static field of a class not yet initialized (the finder must not
// initialize it; the first invocation does).
//
// Run: --jdk-only --nojit with and without the switch, --jdk-only (JIT) and
// --compatible (control). All four must print the block below.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// getter#0 value=3 type=(Box)int fresh=true reveal=getField Box.v ()int
// getter#1 value=3 type=(Box)int fresh=true reveal=getField Box.v ()int
// getter#2 value=3 type=(Box)int fresh=true reveal=getField Box.v ()int
// setter#0 read=10 type=(Box,int)void fresh=true reveal=putField Box.v (int)void
// setter#1 read=11 type=(Box,int)void fresh=true reveal=putField Box.v (int)void
// setter#2 read=12 type=(Box,int)void fresh=true reveal=putField Box.v (int)void
// static-getter#0 value=5 type=()int fresh=true reveal=getStatic S.s ()int
// static-getter#1 value=5 type=()int fresh=true reveal=getStatic S.s ()int
// static-getter#2 value=5 type=()int fresh=true reveal=getStatic S.s ()int
// static-setter#0 read=20 type=(int)void fresh=true reveal=putStatic S.s (int)void
// static-setter#1 read=21 type=(int)void fresh=true reveal=putStatic S.s (int)void
// static-setter#2 read=22 type=(int)void fresh=true reveal=putStatic S.s (int)void
// inherited-static#0 value=11 reveal=getStatic P.p ()int
// inherited-static#1 value=11 reveal=getStatic P.p ()int
// inherited-static#2 value=11 reveal=getStatic P.p ()int
// final-setter#0 java.lang.IllegalAccessException
// final-setter#1 java.lang.IllegalAccessException
// final-setter#2 java.lang.IllegalAccessException
// missing#0 java.lang.NoSuchFieldException
// missing#1 java.lang.NoSuchFieldException
// missing#2 java.lang.NoSuchFieldException
// wrong-type#0 java.lang.NoSuchFieldException
// wrong-type#1 java.lang.NoSuchFieldException
// wrong-type#2 java.lang.NoSuchFieldException
// private-full#0 value=4
// private-public#0 java.lang.IllegalAccessException
// private-full#1 value=4
// private-public#1 java.lang.IllegalAccessException
// private-full#2 value=4
// private-public#2 java.lang.IllegalAccessException
// uninit found=3 initialized=false
// U.<clinit>
// uninit value=9 initialized=true
// uninit again value=9 reveal=getStatic U.u ()int
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;

public class L4W77FinderMint {
    public static class Box {
        public int v = 3;
        public final int f = 1;
        private int priv = 4;
    }

    public static class S {
        public static int s = 5;
    }

    public static class P {
        public static int p = 11;
    }

    public static class Q extends P {
    }

    public static class U {
        static boolean done;
        public static int u = init();

        static int init() {
            System.out.println("U.<clinit>");
            done = true;
            UProbe.seen = true;
            return 9;
        }
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
        Box box = new Box();

        MethodHandle prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle g = lk.findGetter(Box.class, "v", int.class);
            int value = (int) g.invokeExact(box);
            System.out.println("getter#" + i + " value=" + value + " type=" + g.type()
                    + " fresh=" + (g != prev) + " reveal=" + reveal(lk, g));
            prev = g;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle s = lk.findSetter(Box.class, "v", int.class);
            s.invokeExact(box, 10 + i);
            System.out.println("setter#" + i + " read=" + box.v + " type=" + s.type()
                    + " fresh=" + (s != prev) + " reveal=" + reveal(lk, s));
            prev = s;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle g = lk.findStaticGetter(S.class, "s", int.class);
            int value = (int) g.invokeExact();
            System.out.println("static-getter#" + i + " value=" + value + " type=" + g.type()
                    + " fresh=" + (g != prev) + " reveal=" + reveal(lk, g));
            prev = g;
        }
        prev = null;
        for (int i = 0; i < 3; i++) {
            MethodHandle s = lk.findStaticSetter(S.class, "s", int.class);
            s.invokeExact(20 + i);
            System.out.println("static-setter#" + i + " read=" + S.s + " type=" + s.type()
                    + " fresh=" + (s != prev) + " reveal=" + reveal(lk, s));
            prev = s;
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle g = lk.findStaticGetter(Q.class, "p", int.class);
            int value = (int) g.invokeExact();
            System.out.println("inherited-static#" + i + " value=" + value + " reveal=" + reveal(lk, g));
        }
        for (int i = 0; i < 3; i++) {
            try {
                lk.findSetter(Box.class, "f", int.class);
                System.out.println("final-setter#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("final-setter#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            try {
                lk.findGetter(Box.class, "nope", int.class);
                System.out.println("missing#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("missing#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            try {
                lk.findGetter(Box.class, "v", long.class);
                System.out.println("wrong-type#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("wrong-type#" + i + " " + fail(t));
            }
        }
        for (int i = 0; i < 3; i++) {
            MethodHandle g = lk.findGetter(Box.class, "priv", int.class);
            System.out.println("private-full#" + i + " value=" + (int) g.invokeExact(box));
            try {
                pub.findGetter(Box.class, "priv", int.class);
                System.out.println("private-public#" + i + " no exception");
            } catch (Throwable t) {
                System.out.println("private-public#" + i + " " + fail(t));
            }
        }
        MethodHandle u1 = lk.findStaticGetter(U.class, "u", int.class);
        MethodHandle u2 = lk.findStaticGetter(U.class, "u", int.class);
        MethodHandle u3 = lk.findStaticGetter(U.class, "u", int.class);
        int found = (u1 != null ? 1 : 0) + (u2 != null ? 1 : 0) + (u3 != null ? 1 : 0);
        // Reading `U.done` would initialize U: the initializer reports to UProbe.
        System.out.println("uninit found=" + found + " initialized=" + uInitialized());
        int uv = (int) u2.invokeExact();
        System.out.println("uninit value=" + uv + " initialized=" + uInitialized());
        MethodHandle u4 = lk.findStaticGetter(U.class, "u", int.class);
        System.out.println("uninit again value=" + (int) u4.invokeExact() + " reveal=" + reveal(lk, u4));
    }

    /** Whether U ran its initializer, without initializing it. */
    static boolean uInitialized() {
        return UProbe.seen;
    }

    static final class UProbe {
        static boolean seen;
    }
}
