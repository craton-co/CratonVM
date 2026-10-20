// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 96, lane L3 (finding of lane L4, wave 95):
// HotSpot honours `@jdk.internal.reflect.CallerSensitive` only in a class the
// boot or platform loader defines (`ClassFileParser`'s `AnnotationCollector`
// gates it on a privileged loader), never by package name. CratonVM's walk
// tag (`stackwalker::stack_entry_is_caller_sensitive`) screened by NAME
// (`java/`, `jdk/`, `sun/`), so a `sun.*` class a USER loader defines, with
// the annotation in its class file, was tagged ` caller sensitive` in a
// `DROP_METHOD_INFO` walk where HotSpot tags nothing.
//
// The class `sun.l3w96.Sensitive` is built with the JDK 25 ClassFile API (no
// javac flag needed to name the internal annotation) and defined by a user
// `ClassLoader`; its static `run()` carries the annotation and calls `walk`.
// JDK 25 has no `@CallerSensitive` method outside java.*, jdk.* and sun.*
// (searched in src.zip), so no `com.sun.*` row is possible.
//
// Rows:
//   user-sun      -- `Sensitive.run`'s frame in a DROP_METHOD_INFO walk:
//                    not tagged (user loader);
//   boot-control  -- `Method.invoke`'s frame in a DROP_METHOD_INFO +
//                    SHOW_REFLECT_FRAMES walk: tagged (boot class).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; derived from the JDK 25
// sources: ClassFrameInfo.toString is `<class name> <tags>`, as measured in
// L3W55WalkFrameClasses)
// user-sun: [sun.l3w96.Sensitive ]
// boot-control: [java.lang.reflect.Method  caller sensitive]
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W96CallerSensitiveByLoader`.
import java.lang.classfile.Annotation;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.RuntimeVisibleAnnotationsAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;

public class L3W96CallerSensitiveByLoader {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W96CallerSensitiveByLoader.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static List<String> frames(Set<StackWalker.Option> opts) {
        List<String> rows = new ArrayList<>();
        StackWalker.getInstance(opts).forEach(f -> rows.add("[" + f + "]"));
        return rows;
    }

    /** Called by `sun.l3w96.Sensitive.run()`. */
    public static String walk() {
        for (String r : frames(Set.of(StackWalker.Option.DROP_METHOD_INFO))) {
            if (r.startsWith("[sun.l3w96.")) {
                return r;
            }
        }
        return "none";
    }

    /** Reached through `Method.invoke`. */
    public static String reflectedBoot() {
        for (String r : frames(Set.of(StackWalker.Option.DROP_METHOD_INFO,
                StackWalker.Option.SHOW_REFLECT_FRAMES))) {
            if (r.startsWith("[java.lang.reflect.Method ")) {
                return r;
            }
        }
        return "none";
    }

    static void row(String name, java.util.concurrent.Callable<Object> c) {
        String out;
        try {
            out = String.valueOf(c.call());
        } catch (Throwable t) {
            Throwable e = t instanceof java.lang.reflect.InvocationTargetException && t.getCause() != null
                    ? t.getCause() : t;
            out = e.getClass().getName() + ": " + e.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        row("user-sun", () -> {
            ClassDesc self = ClassDesc.of("L3W96CallerSensitiveByLoader");
            byte[] bytes = ClassFile.of().build(ClassDesc.of("sun.l3w96.Sensitive"), cb -> {
                cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
                cb.withMethod("run", MethodTypeDesc.of(ConstantDescs.CD_String),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, mb -> {
                            mb.with(RuntimeVisibleAnnotationsAttribute.of(
                                    Annotation.of(ClassDesc.of("jdk.internal.reflect.CallerSensitive"))));
                            mb.withCode(code -> {
                                code.invokestatic(self, "walk", MethodTypeDesc.of(ConstantDescs.CD_String));
                                code.areturn();
                            });
                        });
            });
            Class<?> c = new Loader().define("sun.l3w96.Sensitive", bytes);
            return c.getMethod("run").invoke(null);
        });
        row("boot-control", () -> {
            Method m = L3W96CallerSensitiveByLoader.class.getMethod("reflectedBoot");
            return m.invoke(null);
        });
    }
}
