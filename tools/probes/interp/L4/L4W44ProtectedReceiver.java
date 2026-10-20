// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 44, lane L4
// (`i43-L4-protected-receiver-restriction-only-on-findgetter`): JDK 25
// `Lookup.restrictProtectedReceiver` / `restrictReceiver` for every finder
// that reaches it. A PROTECTED instance member of a class in another
// run-time package, found through a lookup of a subclass, gets a handle whose
// receiver is the LOOKUP class -- unless the requested class is already
// narrower (`restrictReceiver`'s "already narrow" early return), the lookup is
// trusted (`unreflect` of a `setAccessible(true)` member runs on
// `IMPL_LOOKUP`; the `*-accessible` rows cannot reach that here, since
// `java.base` does not open `java.lang` / `java.io`), or the member is an
// array's inherited `clone`. `findVarHandle`'s coordinate is not covered
// (filed on the page: HotSpot prints `[class L4W44ProtectedReceiver$Sub]`).
//
// Run: javac -d out L4W44ProtectedReceiver.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W44ProtectedReceiver
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   getter: (Sub)int
//   getter-narrower-refc: (SubSub)int
//   getter-invoke: 3
//   setter: (Sub,int)void
//   setter-invoke: 9
//   virtual-clone: (Sub)Object
//   virtual-method: (SubList,int,int)void
//   virtual-invoke: [a, d]
//   virtual-array-clone: (int[])Object
//   unreflect: (Sub)Object
//   unreflect-accessible: not-accessible InaccessibleObjectException
//   unreflect-getter: (Sub)int
//   unreflect-setter: (Sub,int)void
//   unreflect-getter-accessible: not-accessible InaccessibleObjectException
//   unrelated-getter: IllegalAccessException
//
// Before wave 44 CratonVM restricted only `findGetter`, and always to the
// lookup class: `getter-narrower-refc` printed `(Sub)int`, and `setter`,
// `virtual-*` and `unreflect*` printed the declaring class
// (`(ByteArrayOutputStream,int)void`, `(Object)Object`, ...). `setter-invoke`
// and `virtual-invoke` call `invokeExact` at the JDK's restricted type
// (a `WrongMethodTypeException` on the base when the type is not restricted).
import java.io.ByteArrayOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.AbstractList;
import java.util.ArrayList;
import java.util.Arrays;

public class L4W44ProtectedReceiver {
    static class Sub extends ByteArrayOutputStream {
        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    static class SubSub extends Sub {
    }

    static class SubList extends AbstractList<String> {
        final ArrayList<String> items = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));

        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }

        @Override
        public String get(int i) {
            return items.get(i);
        }

        @Override
        public int size() {
            return items.size();
        }

        @Override
        public String remove(int i) {
            return items.remove(i);
        }
    }

    static class Other {
        static MethodHandles.Lookup lookup() {
            return MethodHandles.lookup();
        }
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getSimpleName();
        }
        System.out.println(name + ": " + out);
    }

    static String type(MethodHandle h) {
        return h.type().toString();
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup sub = Sub.lookup();
        row("getter", () -> type(sub.findGetter(ByteArrayOutputStream.class, "count", int.class)));
        row("getter-narrower-refc", () -> type(sub.findGetter(SubSub.class, "count", int.class)));
        row("getter-invoke", () -> {
            Sub s = new Sub();
            s.write(new byte[3], 0, 3);
            MethodHandle g = sub.findGetter(ByteArrayOutputStream.class, "count", int.class);
            return (int) g.invokeExact(s);
        });
        row("setter", () -> type(sub.findSetter(ByteArrayOutputStream.class, "count", int.class)));
        row("setter-invoke", () -> {
            Sub s = new Sub();
            s.write(new byte[16], 0, 16);
            MethodHandle h = sub.findSetter(ByteArrayOutputStream.class, "count", int.class);
            h.invokeExact(s, 9);
            return s.size();
        });
        row("virtual-clone", () -> type(sub.findVirtual(Object.class, "clone", MethodType.methodType(Object.class))));
        MethodHandles.Lookup list = SubList.lookup();
        MethodType rr = MethodType.methodType(void.class, int.class, int.class);
        row("virtual-method", () -> type(list.findVirtual(AbstractList.class, "removeRange", rr)));
        row("virtual-invoke", () -> {
            SubList l = new SubList();
            MethodHandle h = list.findVirtual(AbstractList.class, "removeRange", rr);
            h.invokeExact(l, 1, 3);
            return l.items;
        });
        row("virtual-array-clone",
                () -> type(sub.findVirtual(int[].class, "clone", MethodType.methodType(Object.class))));
        Method clone = Object.class.getDeclaredMethod("clone");
        row("unreflect", () -> type(sub.unreflect(clone)));
        row("unreflect-accessible", () -> {
            Method m = Object.class.getDeclaredMethod("clone");
            try {
                m.setAccessible(true);
            } catch (RuntimeException e) {
                return "not-accessible " + e.getClass().getSimpleName();
            }
            return type(sub.unreflect(m));
        });
        Field count = ByteArrayOutputStream.class.getDeclaredField("count");
        row("unreflect-getter", () -> type(sub.unreflectGetter(count)));
        row("unreflect-setter", () -> type(sub.unreflectSetter(count)));
        row("unreflect-getter-accessible", () -> {
            Field f = ByteArrayOutputStream.class.getDeclaredField("count");
            try {
                f.setAccessible(true);
            } catch (RuntimeException e) {
                return "not-accessible " + e.getClass().getSimpleName();
            }
            return type(sub.unreflectGetter(f));
        });
        row("unrelated-getter", () -> type(Other.lookup().findGetter(ByteArrayOutputStream.class, "count",
                int.class)));
    }
}
