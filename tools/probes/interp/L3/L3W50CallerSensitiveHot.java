// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L3 (review) -- `@CallerSensitive` JDK
// methods (`Reflection.getCallerClass`) called from hot, compiled code and
// from inside stream plumbing. This VM's `Reflection.getCallerClass` reads
// the bare interpreter frame list (`capture_stack_trace`) and skips
// `java/util/stream/`, `Spliterator` and `ServiceLoader` frames by name
// (`native-builtins/src/deprecated_internal.rs`); HotSpot walks every frame
// and skips only `Method.invoke`, the method accessors and method-handle
// adapter frames (`Method::is_ignored_by_security_stack_walk`).
//
// The asking code lives in a class `Twin` defined by its own loader, which
// alone can see `TwinHelper`; a wrong caller answers another loader (the
// application loader cannot find `TwinHelper`). Each row runs 30,000 times
// and reports the count of right answers, then the last answer.
//
// Rows (the first is repeated as the last):
//   lookup-hot          -- `Twin.lookupName()`: `MethodHandles.lookup().lookupClass()`;
//   forname-hot         -- `Twin.find()`: `Class.forName("TwinHelper")`;
//   forname-in-foreach  -- `Twin.find()` from a lambda run by `List.forEach`;
//   forname-in-stream   -- `Twin.find()` from a lambda run by `Stream.map(...).toList()`;
//   lookup-in-stream    -- `MethodHandles.lookup()` in a lambda body of the
//                          probe class, run by `Stream.forEach`;
//   lookup-hot-again    -- `lookup-hot` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// lookup-hot: right=30000/30000 last=Twin
// forname-hot: right=30000/30000 last=TwinHelper
// forname-in-foreach: right=30000/30000 last=TwinHelper
// forname-in-stream: right=30000/30000 last=TwinHelper
// lookup-in-stream: right=30000/30000 last=L3W50CallerSensitiveHot
// lookup-hot-again: right=30000/30000 last=Twin
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W50CallerSensitiveHot`.
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.stream.IntStream;

public class L3W50CallerSensitiveHot {
    static final int N = 30_000;

    static byte[] helper() {
        return ClassFile.of().build(ClassDesc.of("TwinHelper"),
                cb -> cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER));
    }

    /** `Twin`: `lookupName()` and `find()`, each a direct caller-sensitive call. */
    static byte[] twin() {
        ClassDesc lookupDesc = ClassDesc.of("java.lang.invoke.MethodHandles$Lookup");
        return ClassFile.of().build(ClassDesc.of("Twin"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody("lookupName", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.invokestatic(ClassDesc.of("java.lang.invoke.MethodHandles"), "lookup",
                                    MethodTypeDesc.of(lookupDesc))
                            .invokevirtual(lookupDesc, "lookupClass", MethodTypeDesc.of(ConstantDescs.CD_Class))
                            .invokevirtual(ConstantDescs.CD_Class, "getName", MethodTypeDesc.of(ConstantDescs.CD_String))
                            .areturn());
            cb.withMethodBody("find", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                    code -> code.ldc("TwinHelper")
                            .invokestatic(ConstantDescs.CD_Class, "forName",
                                    MethodTypeDesc.of(ConstantDescs.CD_Class, ConstantDescs.CD_String))
                            .invokevirtual(ConstantDescs.CD_Class, "getName", MethodTypeDesc.of(ConstantDescs.CD_String))
                            .areturn());
        });
    }

    interface Ask {
        String ask() throws Throwable;
    }

    static String safe(Ask a) {
        try {
            return a.ask();
        } catch (Throwable t) {
            return t.getClass().getName();
        }
    }

    static void row(String name, String expect, Ask a) {
        int right = 0;
        String last = null;
        for (int i = 0; i < N; i++) {
            last = safe(a);
            if (expect.equals(last)) {
                right++;
            }
        }
        System.out.println(name + ": right=" + right + "/" + N + " last=" + last);
    }

    public static void main(String[] args) throws Throwable {
        byte[] tb = twin();
        byte[] hb = helper();
        ClassLoader l = new ClassLoader("twins", L3W50CallerSensitiveHot.class.getClassLoader()) {
            @Override
            protected Class<?> findClass(String n) throws ClassNotFoundException {
                byte[] b = n.equals("Twin") ? tb : n.equals("TwinHelper") ? hb : null;
                if (b == null) {
                    throw new ClassNotFoundException(n);
                }
                return defineClass(n, b, 0, b.length);
            }
        };
        Class<?> t = Class.forName("Twin", true, l);
        Method lookupName = t.getMethod("lookupName");
        Method find = t.getMethod("find");
        // Direct calls through a MethodHandle so the hot loop is compiled
        // straight-line code (no reflective frames in between).
        var lh = MethodHandles.lookup().unreflect(lookupName);
        var fh = MethodHandles.lookup().unreflect(find);
        row("lookup-hot", "Twin", () -> (String) lh.invokeExact());
        row("forname-hot", "TwinHelper", () -> (String) fh.invokeExact());
        List<Integer> one = List.of(1);
        row("forname-in-foreach", "TwinHelper", () -> {
            String[] out = new String[1];
            one.forEach(x -> out[0] = safe(() -> (String) fh.invokeExact()));
            return out[0];
        });
        row("forname-in-stream", "TwinHelper", () -> one.stream()
                .map(x -> safe(() -> (String) fh.invokeExact())).toList().get(0));
        row("lookup-in-stream", "L3W50CallerSensitiveHot", () -> {
            List<String> out = new ArrayList<>();
            IntStream.of(1).forEach(x -> out.add(MethodHandles.lookup().lookupClass().getName()));
            return out.get(0);
        });
        row("lookup-hot-again", "Twin", () -> (String) lh.invokeExact());
    }
}
