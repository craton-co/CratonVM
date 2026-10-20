// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L3 (proposal
// `i37-L6-proposal-hidden-method-bit-at-link-time`, its privilege half;
// `CRATONVM_HIDDEN_ANNOTATION_PRIVILEGED_ONLY`, default ON, `=0` off):
// `@jdk.internal.vm.annotation.Hidden` on a method of a `sun.*` class that an
// APPLICATION class loader defined. HotSpot's `ClassFileParser` honours the
// annotation only in privileged code (the boot and platform loaders' classes),
// so the frame stays in every trace and walk. CratonVM used to decide it by the
// class NAME (`java/`, `jdk/`, `sun/`) and hid it.
//
// The class `sun.l3w115.Carrier` is embedded below (javac 25 with
// `--add-exports java.base/jdk.internal.vm.annotation=ALL-UNNAMED -g:none` of
//     package sun.l3w115;
//     public class Carrier {
//         @jdk.internal.vm.annotation.Hidden
//         public static void run(Runnable r) { r.run(); }
//     }
// ) and defined by a plain URLClassLoader-like child of the application loader.
// Each row lists the frames of this probe and of the carrier, innermost first.
//
// Run: javac -d . L3W115UserHiddenAnnotation.java
//      java -cp . L3W115UserHiddenAnnotation                          (HotSpot reference)
//      cratonvm --jdk-only -cp . L3W115UserHiddenAnnotation
//      cratonvm --compatible -cp . L3W115UserHiddenAnnotation
//      (JIT and --nojit; CRATONVM_HIDDEN_ANNOTATION_PRIVILEGED_ONLY=0 is the
//      old answer: the sun.l3w115.Carrier::run frame missing, under --jdk-only, from every row
//      but `walk-hidden`)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// loader app=true
// trace: L3W115UserHiddenAnnotation::lambda$main$N | sun.l3w115.Carrier::run | L3W115UserHiddenAnnotation::main
// walk-default: L3W115UserHiddenAnnotation::walk | L3W115UserHiddenAnnotation::lambda$main$N | sun.l3w115.Carrier::run | L3W115UserHiddenAnnotation::main
// walk-hidden: L3W115UserHiddenAnnotation::walk | L3W115UserHiddenAnnotation::lambda$main$N | L3W115UserHiddenAnnotation$$Lambda::run | sun.l3w115.Carrier::run | L3W115UserHiddenAnnotation::main
// thread: L3W115UserHiddenAnnotation::lambda$main$N | sun.l3w115.Carrier::run | L3W115UserHiddenAnnotation::main
// HOTSPOT_EXPECTED_END

import java.lang.StackWalker.Option;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Base64;
import java.util.List;

public class L3W115UserHiddenAnnotation {
    static final String CARRIER =
            "yv66vgAAAEUAEgoAAgADBwAEDAAFAAYBABBqYXZhL2xhbmcvT2JqZWN0AQAGPGluaXQ+AQADKClWCwAIAAkHAAoMAAsABgEAEmphdmEvbGFuZy9SdW5uYWJsZQEAA3J1bgcADQEAEnN1bi9sM3cxMTUvQ2FycmllcgEABENvZGUBABcoTGphdmEvbGFuZy9SdW5uYWJsZTspVgEAGVJ1bnRpbWVWaXNpYmxlQW5ub3RhdGlvbnMBACNMamRrL2ludGVybmFsL3ZtL2Fubm90YXRpb24vSGlkZGVuOwAhAAwAAgAAAAAAAgABAAUABgABAA4AAAARAAEAAQAAAAUqtwABsQAAAAAACQALAA8AAgAOAAAAEwABAAEAAAAHKrkABwEAsQAAAAAAEAAAAAYAAQARAAAAAA==";

    static final class Loader extends ClassLoader {
        Loader(ClassLoader parent) {
            super(parent);
        }

        Class<?> define(byte[] bytes) {
            return defineClass("sun.l3w115.Carrier", bytes, 0, bytes.length);
        }
    }

    static boolean mine(String className) {
        return className.startsWith("L3W115") || className.startsWith("sun.l3w115.");
    }

    static String fold(String s) {
        return s.replaceAll("lambda\\$(\\w+)\\$\\d+", "lambda\\$$1\\$N")
                .replaceAll("\\$\\$Lambda[/$]?[0-9a-zA-Z]*", "\\$\\$Lambda");
    }

    static String trace;
    static String walkDefault;
    static String walkHidden;
    static String thread;

    static String join(List<String> frames) {
        return String.join(" | ", frames);
    }

    static List<String> of(StackTraceElement[] elements) {
        List<String> out = new ArrayList<>();
        for (StackTraceElement e : elements) {
            if (mine(e.getClassName())) {
                out.add(fold(e.getClassName() + "::" + e.getMethodName()));
            }
        }
        return out;
    }

    static List<String> walk(StackWalker walker) {
        return walker.walk(s -> {
            List<String> out = new ArrayList<>();
            s.forEach(f -> {
                if (mine(f.getClassName())) {
                    out.add(fold(f.getClassName() + "::" + f.getMethodName()));
                }
            });
            return out;
        });
    }

    public static void main(String[] args) throws Throwable {
        Loader loader = new Loader(L3W115UserHiddenAnnotation.class.getClassLoader());
        Class<?> carrier = loader.define(Base64.getDecoder().decode(CARRIER));
        System.out.println("loader app=" + (carrier.getClassLoader() == loader));
        Method run = carrier.getMethod("run", Runnable.class);
        Runnable body = () -> {
            trace = join(of(new Throwable().getStackTrace()));
            walkDefault = join(walk(StackWalker.getInstance()));
            walkHidden = join(walk(StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES)));
            thread = join(of(Thread.currentThread().getStackTrace()));
        };
        run.invoke(null, body);
        System.out.println("trace: " + trace);
        System.out.println("walk-default: " + walkDefault);
        System.out.println("walk-hidden: " + walkHidden);
        System.out.println("thread: " + thread);
    }
}
