// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the compiled reflective links
// (`CRATONVM_JIT_REFLECT_ACCESSOR_LINK`, `CRATONVM_JIT_REFLECT_CTOR_LINK`,
// `CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN`) and the reflective sig-poly rule
// (`CRATONVM_REFLECT_SIG_POLY_UOE`, default ON).
//
// Risk targeted: a link that replaces `DirectMethodHandleAccessor.invokeImpl`'s
// chain must keep every Java-visible answer of the chain:
//   * argument conversion: widening (int -> long / double), unboxing of
//     every wrapper, `null` for a primitive parameter (IllegalArgumentException),
//     a wrong reference type (IllegalArgumentException with the JDK text),
//     a wrong argument count;
//   * return boxing: the box caches (Integer -128..127, Boolean.TRUE, Byte,
//     Character <= 127, Short, Long) are identity-visible;
//   * exceptions thrown by the target wrapped in InvocationTargetException
//     (cause type and message), a null receiver (NPE) and a receiver of the
//     wrong class (IllegalArgumentException);
//   * virtual dispatch through a Method of the superclass on a subclass
//     receiver, an interface default method, a private method;
//   * Constructor.newInstance: arguments pinned across the allocation (GC
//     churn in between), a throwing constructor (ITE), an abstract class
//     (InstantiationException), a class whose <clinit> runs on first newInstance;
//   * `MethodHandle.invoke` / `invokeExact` called reflectively:
//     UnsupportedOperationException (JDK 25 wording).
// Every Method / Constructor is called hot (> 50k times) then once per edge.
//
// Run:
//   javac -d out R21FzReflInvoke1.java
//   java -Xmx256m -cp out R21FzReflInvoke1                                # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzReflInvoke1
//   CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1 cratonvm ...; CRATONVM_JIT_REFLECT_CTOR_LINK=1 cratonvm ...
//   CRATONVM_JIT_MH_COMPILED_LINK_HIDDEN=1 cratonvm ...; CRATONVM_REFLECT_SIG_POLY_UOE=0 cratonvm ... (kill)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class R21FzReflInvoke1 {
    static final int WARM = 60_000;

    public static int sInt(int a) {
        return a + 1;
    }

    public static long sLong(long a, int b) {
        return a * b;
    }

    public static double sDouble(double a) {
        return a / 2;
    }

    public static boolean sBool(Object o) {
        return o != null;
    }

    public static Integer sBox(int a) {
        return a;
    }

    public static char sChar(int a) {
        return (char) a;
    }

    public static String sStr(String a, int b, Object c) {
        return a + b + c;
    }

    public static void sThrow(int a) {
        if (a > 0) {
            throw new IllegalStateException("thrown " + a);
        }
    }

    public interface Greeter {
        default String greet() {
            return "hi from " + getClass().getSimpleName();
        }
    }

    public static class Base implements Greeter {
        public int v(int x) {
            return x;
        }

        private int hidden(int x) {
            return -x;
        }
    }

    public static class Sub extends Base {
        @Override
        public int v(int x) {
            return x * 10;
        }
    }

    public static class Pt {
        final int x;
        final Object y;

        public Pt(int x, Object y) {
            if (x < 0) {
                throw new IllegalArgumentException("neg " + x);
            }
            this.x = x;
            this.y = y;
        }

        @Override
        public String toString() {
            return "Pt(" + x + "," + y + ")";
        }
    }

    public abstract static class Abs {
        public Abs() {
        }
    }

    public static class LazyInit {
        static String log = "";

        static {
            log += "[clinit]";
        }

        public LazyInit() {
            log += "[ctor]";
        }
    }

    static String call(Method m, Object recv, Object... args) {
        try {
            Object r = m.invoke(recv, args);
            return r == null ? "null" : r.getClass().getSimpleName() + ":" + r;
        } catch (InvocationTargetException e) {
            return "ITE(" + e.getCause().getClass().getName() + ":" + e.getCause().getMessage() + ")";
        } catch (IllegalArgumentException | NullPointerException | UnsupportedOperationException
                | IllegalAccessException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static String make(Constructor<?> c, Object... args) {
        try {
            return c.newInstance(args).toString().replaceAll("@[0-9a-f]+", "@");
        } catch (InvocationTargetException e) {
            return "ITE(" + e.getCause().getClass().getName() + ":" + e.getCause().getMessage() + ")";
        } catch (IllegalArgumentException | InstantiationException | IllegalAccessException e) {
            return e.getClass().getName() + ":" + e.getMessage();
        }
    }

    static Method m(String name, Class<?>... ps) throws Exception {
        return R21FzReflInvoke1.class.getMethod(name, ps);
    }

    static void rows(String tag) throws Exception {
        Method sInt = m("sInt", int.class);
        Method sLong = m("sLong", long.class, int.class);
        Method sDouble = m("sDouble", double.class);
        Method sBool = m("sBool", Object.class);
        Method sBox = m("sBox", int.class);
        Method sChar = m("sChar", int.class);
        Method sStr = m("sStr", String.class, int.class, Object.class);
        Method sThrow = m("sThrow", int.class);
        Method v = Base.class.getMethod("v", int.class);
        Method greet = Greeter.class.getMethod("greet");
        Method hidden = Base.class.getDeclaredMethod("hidden", int.class);
        hidden.setAccessible(true);
        String[] r = {
            call(sInt, null, 5), call(sInt, null, (byte) 5), call(sInt, null, 'a'), call(sInt, null, 5L),
            call(sInt, null, (Object) null), call(sInt, null, "x"), call(sInt, null), call(sInt, null, 1, 2),
            call(sLong, null, 3, 4), call(sLong, null, Long.MAX_VALUE, 2), call(sDouble, null, 7),
            call(sDouble, null, 1.5f), call(sBool, null, (Object) null), call(sBool, null, "o"),
            call(sBox, null, 127), call(sBox, null, 128), call(sChar, null, 66), call(sStr, null, "a", 1, null),
            call(sStr, null, null, 2, 3.0), call(sThrow, null, 0), call(sThrow, null, 9),
            call(v, new Base(), 3), call(v, new Sub(), 3), call(v, null, 3), call(v, "notbase", 3),
            call(greet, new Sub()), call(hidden, new Sub(), 4),
        };
        StringBuilder sb = new StringBuilder("row " + tag);
        for (String s : r) {
            sb.append(" | ").append(s);
        }
        System.out.println(sb);
        Method i1 = m("sBox", int.class);
        Object a = i1.invoke(null, 100);
        Object b = i1.invoke(null, 100);
        Object c = i1.invoke(null, 1000);
        Object d = i1.invoke(null, 1000);
        Object t1 = sBool.invoke(null, "x");
        System.out.println("row " + tag + " identity " + (a == b) + " " + (c == d) + " " + (t1 == Boolean.TRUE));
        Constructor<Pt> pt = Pt.class.getConstructor(int.class, Object.class);
        Constructor<Abs> abs = Abs.class.getConstructor();
        System.out.println("row " + tag + " ctor " + make(pt, 3, "y") + " " + make(pt, -1, null) + " "
                + make(pt, (Object) null, "z") + " " + make(pt, 1) + " " + make(abs));
        Method invoke = MethodHandle.class.getMethod("invoke", Object[].class);
        Method invokeExact = MethodHandle.class.getMethod("invokeExact", Object[].class);
        MethodHandle mh = MethodHandles.lookup().findStatic(R21FzReflInvoke1.class, "sInt",
                MethodType.methodType(int.class, int.class));
        System.out.println("row " + tag + " sigpoly " + call(invoke, mh, (Object) new Object[] {1}) + " "
                + call(invokeExact, mh, (Object) new Object[] {1}));
    }

    static int churn(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += new int[4].length;
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        rows("cold");
        Method sInt = m("sInt", int.class);
        Method sStr = m("sStr", String.class, int.class, Object.class);
        Method v = Base.class.getMethod("v", int.class);
        Constructor<Pt> pt = Pt.class.getConstructor(int.class, Object.class);
        Base[] recv = {new Base(), new Sub()};
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + (Integer) sInt.invoke(null, r);
            h = h * 31 + ((String) sStr.invoke(null, "k", r & 7, null)).length();
            h = h * 31 + (Integer) v.invoke(recv[r & 1], r);
            Pt p = pt.newInstance(r & 1023, "q" + (r & 3));
            if ((r & 4095) == 0) {
                churn(100_000);
            }
            h = h * 31 + p.x + p.y.hashCode();
        }
        System.out.println("warm hash " + h);
        rows("hot");
        Constructor<LazyInit> lazy = LazyInit.class.getConstructor();
        System.out.println("lazy " + make(lazy).replaceAll("LazyInit@.*", "LazyInit@") + " " + LazyInit.log);
        rows("after");
    }
}
