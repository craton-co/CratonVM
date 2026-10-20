// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 55, lane L3 -- the classes of the frames a
// `StackWalker` walk hands out, and the frames on top of what their
// accessors raise. HotSpot hands out a `java.lang.StackFrameInfo`, or a
// `java.lang.ClassFrameInfo` for a `DROP_METHOD_INFO` walker, and their
// raises start in the JDK's bodies. `--compatible`'s native walk handed out
// the VM's own carrier (an instance of the `StackWalker$StackFrame`
// INTERFACE): every `DROP_METHOD_INFO` accessor answered, and a raise listed
// no JDK frame (the served-raise mark selected the interface's abstract
// method from the carrier).
//
// Each raise row prints the throwable's class and message, the frames above
// the probe's first frame as `class.method`, then the first probe frame's class.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   walk-class / forEach-class / drop-class  -- the frame's class;
//   declaring        -- `getDeclaringClass()` of a plain walker's frame;
//   method-type      -- `getMethodType()` of a plain walker's frame;
//   descriptor       -- `getDescriptor()` of a plain walker's frame;
//   drop-method      -- `getMethodName()` of a DROP_METHOD_INFO frame;
//   drop-line / drop-element -- `getLineNumber()` / `toStackTraceElement()`;
//   drop-declaring   -- `getDeclaringClass()` of a DROP_METHOD_INFO frame;
//   drop-retain-method-type / drop-retain-descriptor -- the interface's
//                       default methods, on a DROP_METHOD_INFO +
//                       RETAIN_CLASS_REFERENCE frame;
//   drop-string      -- `toString()` of a DROP_METHOD_INFO frame;
//   caller-sensitive -- a DROP_METHOD_INFO + SHOW_REFLECT_FRAMES walk from a
//                       method `Method.invoke` called: `Method.invoke`'s frame
//                       is tagged ` caller sensitive`;
//   declaring-again  -- `declaring` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// declaring: java.lang.UnsupportedOperationException No access to RETAIN_CLASS_REFERENCE [java.lang.ClassFrameInfo.ensureRetainClassRefEnabled java.lang.ClassFrameInfo.getDeclaringClass] L3W55WalkFrameClasses
// walk-class: java.lang.StackFrameInfo
// forEach-class: java.lang.StackFrameInfo
// drop-class: java.lang.ClassFrameInfo
// method-type: java.lang.UnsupportedOperationException No access to RETAIN_CLASS_REFERENCE [java.lang.ClassFrameInfo.ensureRetainClassRefEnabled java.lang.StackFrameInfo.getMethodType] L3W55WalkFrameClasses
// descriptor: java.lang.UnsupportedOperationException No access to RETAIN_CLASS_REFERENCE [java.lang.ClassFrameInfo.ensureRetainClassRefEnabled java.lang.StackFrameInfo.getMethodType java.lang.StackFrameInfo.getDescriptor] L3W55WalkFrameClasses
// drop-method: java.lang.UnsupportedOperationException null [java.lang.ClassFrameInfo.getMethodName] L3W55WalkFrameClasses
// drop-line: java.lang.UnsupportedOperationException null [java.lang.ClassFrameInfo.getLineNumber] L3W55WalkFrameClasses
// drop-element: java.lang.UnsupportedOperationException null [java.lang.ClassFrameInfo.toStackTraceElement] L3W55WalkFrameClasses
// drop-declaring: java.lang.UnsupportedOperationException No access to RETAIN_CLASS_REFERENCE [java.lang.ClassFrameInfo.ensureRetainClassRefEnabled java.lang.ClassFrameInfo.getDeclaringClass] L3W55WalkFrameClasses
// drop-retain-method-type: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getMethodType] L3W55WalkFrameClasses
// drop-retain-descriptor: java.lang.UnsupportedOperationException null [java.lang.StackWalker$StackFrame.getDescriptor] L3W55WalkFrameClasses
// drop-string: [L3W55WalkFrameClasses ]
// caller-sensitive: [L3W55WalkFrameClasses ] [java.lang.reflect.Method  caller sensitive] [L3W55WalkFrameClasses ]
// declaring-again: java.lang.UnsupportedOperationException No access to RETAIN_CLASS_REFERENCE [java.lang.ClassFrameInfo.ensureRetainClassRefEnabled java.lang.ClassFrameInfo.getDeclaringClass] L3W55WalkFrameClasses
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W55WalkFrameClasses`.
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.concurrent.Callable;

public class L3W55WalkFrameClasses {
    static void raise(String name, Callable<Object> c) {
        try {
            System.out.println(name + ": " + c.call());
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            StringBuilder jdk = new StringBuilder();
            int i = 0;
            while (i < st.length && !st[i].getClassName().startsWith("L3W55")) {
                jdk.append(i == 0 ? "" : " ").append(st[i].getClassName()).append('.').append(st[i].getMethodName());
                i++;
            }
            String first = i < st.length ? st[i].getClassName() : "-";
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage() + " [" + jdk + "] " + first);
        }
    }

    static StackWalker.StackFrame first(StackWalker w) {
        return w.walk(s -> s.findFirst().get());
    }

    /** Reached through `Method.invoke`: a DROP_METHOD_INFO walk's first three
     *  frames with the reflection frames shown, hidden ones not. */
    public static String reflected() {
        List<String> rows = new ArrayList<>();
        StackWalker.getInstance(Set.of(StackWalker.Option.DROP_METHOD_INFO, StackWalker.Option.SHOW_REFLECT_FRAMES))
                .forEach(f -> rows.add("[" + f + "]"));
        StringBuilder sb = new StringBuilder();
        int n = 0;
        for (String r : rows) {
            if (r.contains("Accessor")) {
                continue;
            }
            sb.append(n == 0 ? "" : " ").append(r);
            if (++n == 3) {
                break;
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        StackWalker plain = StackWalker.getInstance();
        StackWalker.StackFrame p = first(plain);
        raise("declaring", () -> p.getDeclaringClass());
        System.out.println("walk-class: " + p.getClass().getName());
        List<StackWalker.StackFrame> each = new ArrayList<>();
        plain.forEach(each::add);
        System.out.println("forEach-class: " + each.get(0).getClass().getName());
        StackWalker.StackFrame d = first(StackWalker.getInstance(StackWalker.Option.DROP_METHOD_INFO));
        System.out.println("drop-class: " + d.getClass().getName());
        raise("method-type", () -> p.getMethodType());
        raise("descriptor", () -> p.getDescriptor());
        raise("drop-method", () -> d.getMethodName());
        raise("drop-line", () -> d.getLineNumber());
        raise("drop-element", () -> d.toStackTraceElement());
        raise("drop-declaring", () -> d.getDeclaringClass());
        StackWalker.StackFrame dr = first(StackWalker.getInstance(
                Set.of(StackWalker.Option.DROP_METHOD_INFO, StackWalker.Option.RETAIN_CLASS_REFERENCE)));
        raise("drop-retain-method-type", () -> dr.getMethodType());
        raise("drop-retain-descriptor", () -> dr.getDescriptor());
        System.out.println("drop-string: [" + d + "]");
        Method m = L3W55WalkFrameClasses.class.getMethod("reflected");
        System.out.println("caller-sensitive: " + m.invoke(null));
        raise("declaring-again", () -> p.getDeclaringClass());
    }
}
