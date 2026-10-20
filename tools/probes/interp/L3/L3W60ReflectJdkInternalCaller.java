// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L3 -- the caller-class trap wave 59 left
// (`docs/known-issues/interpreter/interpreter-round-i1-handoff-20260929.md`
// "Wave-59 lessons"): the served reflective doors resolve their caller with
// `resolve_caller_class_id`, which skips every `java.lang.reflect` /
// `java.lang.invoke` frame, and `CRATONVM_REFLECT_CLASS_ACCESS` (default on)
// now REFUSES a non-public class whose runtime package is not the caller's.
// Every row reaches a NON-PUBLIC class of this probe's package through JDK
// code in those packages (or through a method handle, whose caller-sensitive
// binding names this class), which HotSpot allows; the last rows are the
// refusal control (a non-public class of another package).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// enum-valueOf: RED
// enum-class-valueOf: GREEN
// proxy-package-private-interface: hello proxy
// proxy-invokeDefault: default hi
// serialized-lambda: deserialized ran
// handle-to-method-invoke: hidden hello
// reflect-on-method-invoke: hidden hello
// handle-to-constructor-newInstance: made
// reflect-on-constructor-newInstance: made
// field-get-through-handle: 42
// refused-foreign: java.lang.IllegalAccessException: class L3W60ReflectJdkInternalCaller cannot access a member of class java.util.ImmutableCollections$ListN (in module java.base) with modifiers "public"
// enum-valueOf-again: RED
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W60ReflectJdkInternalCaller`.
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.util.List;

public class L3W60ReflectJdkInternalCaller {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            Throwable c = t instanceof InvocationTargetException && t.getCause() != null ? t.getCause() : t;
            System.out.println(name + ": " + c);
        }
    }

    enum Color {
        RED,
        GREEN
    }

    interface Greeter {
        String greet();

        default String hi() {
            return "default hi";
        }
    }

    static class Hidden {
        public int value = 42;

        public Hidden() {}

        public String hello() {
            return "hidden hello";
        }

        @Override
        public String toString() {
            return "made";
        }
    }

    static class Holder {
        static Runnable make() {
            return (Runnable & Serializable) () -> System.out.print("");
        }
    }

    public static void main(String[] args) throws Throwable {
        row("enum-valueOf", () -> Enum.valueOf(Color.class, "RED"));
        row("enum-class-valueOf", () -> Color.valueOf("GREEN"));
        row("proxy-package-private-interface", () -> {
            Greeter g = (Greeter) Proxy.newProxyInstance(Greeter.class.getClassLoader(),
                    new Class<?>[] {Greeter.class}, (p, m, a) -> "hello proxy");
            return g.greet();
        });
        row("proxy-invokeDefault", () -> {
            InvocationHandler h = (p, m, a) -> m.isDefault() ? InvocationHandler.invokeDefault(p, m, a) : "x";
            Greeter g = (Greeter) Proxy.newProxyInstance(Greeter.class.getClassLoader(),
                    new Class<?>[] {Greeter.class}, h);
            return g.hi();
        });
        row("serialized-lambda", () -> {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            try (ObjectOutputStream out = new ObjectOutputStream(bytes)) {
                out.writeObject(Holder.make());
            }
            try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bytes.toByteArray()))) {
                Runnable r = (Runnable) in.readObject();
                r.run();
            }
            return "deserialized ran";
        });
        Method hello = Hidden.class.getMethod("hello");
        Constructor<Hidden> ctor = Hidden.class.getConstructor();
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        row("handle-to-method-invoke", () -> {
            MethodHandle invoke = lookup.findVirtual(Method.class, "invoke",
                    MethodType.methodType(Object.class, Object.class, Object[].class));
            return invoke.invoke(hello, new Hidden(), new Object[0]);
        });
        row("reflect-on-method-invoke", () -> {
            Method invoke = Method.class.getMethod("invoke", Object.class, Object[].class);
            return invoke.invoke(hello, new Hidden(), new Object[0]);
        });
        row("handle-to-constructor-newInstance", () -> {
            MethodHandle newInstance = lookup.findVirtual(Constructor.class, "newInstance",
                    MethodType.methodType(Object.class, Object[].class));
            return newInstance.invoke(ctor, new Object[0]);
        });
        row("reflect-on-constructor-newInstance", () -> {
            Method newInstance = Constructor.class.getMethod("newInstance", Object[].class);
            return newInstance.invoke(ctor, (Object) new Object[0]);
        });
        row("field-get-through-handle", () -> {
            Field value = Hidden.class.getField("value");
            MethodHandle get = lookup.findVirtual(Field.class, "get",
                    MethodType.methodType(Object.class, Object.class));
            return get.invoke(value, new Hidden());
        });
        row("refused-foreign", () -> {
            List<Integer> list = List.of(1, 2, 3);
            return list.getClass().getMethod("size").invoke(list);
        });
        row("enum-valueOf-again", () -> Enum.valueOf(Color.class, "RED"));
    }
}
