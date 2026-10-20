// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: `NativeAccessor.invoke0` checks with the
// VM's texts (`CRATONVM_REFLECT_INVOKE0_VM_TEXTS`, default ON;
// `lang_class.rs` `native_method_invoke_body`, `vm_texts = !gate_access && ...`:
// a message-less NPE, "object is not an instance of declaring class",
// "wrong number of arguments") and the reflective signature-polymorphic
// refusal (`CRATONVM_REFLECT_SIG_POLY_UOE`).
//
// Risk targeted: `-Djdk.reflect.useNativeAccessorOnly=true` routes every
// Method.invoke / Constructor.newInstance through the native accessors,
// whose checks are HotSpot's `Reflection::invoke`, not the Java accessor's
// `checkReceiver` / `checkArgumentCount`. Rows: null receiver, wrong
// receiver class, wrong arity (both directions), argument type mismatch,
// null for a primitive, widening (int -> long, char -> int), unboxing, an
// exception from the target (InvocationTargetException, same cause), a
// static method with a junk receiver (ignored), a private method after
// setAccessible, an interface default method, an overridden method through
// the superclass's Method (virtual dispatch), MethodHandle.invoke through
// reflection (UnsupportedOperationException), constructors (wrong arity /
// type). Each prints the exception class and message. Cold, hot (50k calls
// per row), a deopt trigger (a subclass receiver), again.
//
// Run (the system property on BOTH VMs):
//   javac -d out R21Fz2ReflNative1.java
//   java -Djdk.reflect.useNativeAccessorOnly=true -Xmx256m -cp out R21Fz2ReflNative1   # HotSpot 25
//   cratonvm -Djdk.reflect.useNativeAccessorOnly=true -Xmx256m -cp out R21Fz2ReflNative1
//   CRATONVM_REFLECT_INVOKE0_VM_TEXTS=0 cratonvm -Djdk.reflect.useNativeAccessorOnly=true ...  (kill)
//   CRATONVM_JIT_REFLECT_ACCESSOR_LINK=1 cratonvm -Djdk.reflect.useNativeAccessorOnly=true ...
//   and once WITHOUT the property on both VMs (the Java accessor texts)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm (a kill arm may show the old texts).

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Constructor;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

public class R21Fz2ReflNative1 {
    static final int WARM = 50_000;

    interface Greeter {
        default String greet(String who) {
            return "hi " + who;
        }
    }

    public static class Base implements Greeter {
        public int add(int a, long b) {
            return (int) (a + b);
        }

        public int code(char c) {
            return c;
        }

        public String who() {
            return "base";
        }

        public static int twice(int x) {
            return x * 2;
        }

        public void boom(int k) {
            if (k > 0) {
                throw new IllegalStateException("boom" + k);
            }
        }

        private int secret() {
            return 42;
        }

        public Base() {
        }

        public Base(int x, String s) {
        }
    }

    public static class Sub extends Base {
        @Override
        public String who() {
            return "sub";
        }
    }

    public static class Late extends Sub {
        @Override
        public String who() {
            return "late";
        }
    }

    static String inv(Method m, Object recv, Object... args) {
        try {
            return String.valueOf(m.invoke(recv, args));
        } catch (InvocationTargetException e) {
            Throwable c = e.getCause();
            return "ITE(" + c.getClass().getSimpleName() + ":" + c.getMessage() + ")";
        } catch (NullPointerException e) {
            // Message printed: invoke0's NPE has none on HotSpot.
            return "NPE(" + e.getMessage() + ")";
        } catch (IllegalArgumentException | UnsupportedOperationException | IllegalAccessException e) {
            return e.getClass().getSimpleName() + "(" + e.getMessage() + ")";
        }
    }

    static String ctor(Constructor<?> c, Object... args) {
        try {
            return c.newInstance(args).getClass().getSimpleName();
        } catch (InvocationTargetException e) {
            return "ITE(" + e.getCause() + ")";
        } catch (IllegalArgumentException | ReflectiveOperationException e) {
            return e.getClass().getSimpleName() + "(" + e.getMessage() + ")";
        }
    }

    public static void main(String[] args) throws Exception {
        Method add = Base.class.getMethod("add", int.class, long.class);
        Method code = Base.class.getMethod("code", char.class);
        Method who = Base.class.getMethod("who");
        Method twice = Base.class.getMethod("twice", int.class);
        Method boom = Base.class.getMethod("boom", int.class);
        Method secret = Base.class.getDeclaredMethod("secret");
        secret.setAccessible(true);
        Method greet = Greeter.class.getMethod("greet", String.class);
        Method mhInvoke = MethodHandle.class.getMethod("invoke", Object[].class);
        Constructor<Base> c0 = Base.class.getConstructor();
        Constructor<Base> c2 = Base.class.getConstructor(int.class, String.class);
        MethodHandle mh = MethodHandles.lookup().findStatic(Base.class, "twice",
                java.lang.invoke.MethodType.methodType(int.class, int.class));
        Base b = new Base();
        Base s = new Sub();
        for (int pass = 0; pass < 3; pass++) {
            String tag = pass == 0 ? "cold" : pass == 1 ? "hot" : "after";
            System.out.println("row " + tag + " | " + inv(add, b, 1, 2L) + " " + inv(add, b, 1, 2) + " "
                    + inv(add, b, 'x', (short) 3) + " " + inv(add, null, 1, 2L) + " " + inv(add, "str", 1, 2L) + " "
                    + inv(add, b, 1) + " " + inv(add, b, 1, 2L, 3) + " " + inv(add, b, "1", 2L) + " "
                    + inv(add, b, null, 2L) + " " + inv(add, b, 1.5, 2L) + " " + inv(code, b, 'A') + " "
                    + inv(code, b, 65) + " " + inv(who, s) + " " + inv(who, b) + " " + inv(twice, "junk", 21) + " "
                    + inv(twice, null, 21) + " " + inv(boom, b, 3) + " " + inv(boom, b, 0) + " " + inv(secret, s)
                    + " " + inv(greet, b, "you") + " " + inv(greet, "notGreeter", "x") + " "
                    + inv(mhInvoke, mh, (Object) new Object[] {5}) + " | " + ctor(c0) + " " + ctor(c0, 1) + " "
                    + ctor(c2, 1, "s") + " " + ctor(c2, "1", "s") + " " + ctor(c2, null, "s"));
            if (pass == 0) {
                long h = 0;
                for (int r = 0; r < WARM; r++) {
                    h = h * 31 + inv(add, b, r, 2L).length() + inv(who, (r & 1) == 0 ? b : s).length()
                            + inv(twice, null, r).length();
                    if ((r & 31) == 0) {
                        h = h * 31 + inv(add, "str", 1, 2L).length() + inv(add, b, 1).length()
                                + inv(add, null, 1, 2L).length() + inv(boom, b, 1).length();
                    }
                }
                System.out.println("warm hash " + h);
                // Deopt trigger: a receiver class the hot sites never saw.
                Base late = new Late();
                System.out.println("late " + inv(who, late) + " " + inv(add, late, 4, 5L) + " " + inv(secret, late));
                s = late;
            }
        }
    }
}
