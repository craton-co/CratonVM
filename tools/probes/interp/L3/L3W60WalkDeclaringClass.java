// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L3 (review): a `StackWalker` frame's
// `getClassName()` runs the JDK's own `StackFrameInfo.declaringClass()`,
// `JLIA.getDeclaringClass(classOrMemberName)`, a cast to
// `ResolvedMethodName`. The walk's frames built under
// `CRATONVM_SFI_WALK_LINES` (`lang_stackwalker::populate_sfi_from_walk_lines`)
// stored the bare `Class` there unless the pure-bytecode lever was set, so on
// the base every `--jdk-only` walk that read a class name raised
// `ClassCastException: class java.lang.Class cannot be cast to class
// java.lang.invoke.ResolvedMethodName` (`L3W43ReflectionFrames` `walk-*`
// rows). The first row is repeated last.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// walk-names: L3W60WalkDeclaringClass.names,L3W60WalkDeclaringClass.lambda$main$0,L3W60WalkDeclaringClass.row
// walk-retain-declaring: L3W60WalkDeclaringClass,L3W60WalkDeclaringClass,L3W60WalkDeclaringClass
// walk-forEach: L3W60WalkDeclaringClass.forEachNames
// walk-reflective: L3W60WalkDeclaringClass.names,jdk.internal.reflect.DirectMethodHandleAccessor.invoke
// walk-toString: L3W60WalkDeclaringClass.lambda$main$5
// walk-names-again: L3W60WalkDeclaringClass.names,L3W60WalkDeclaringClass.lambda$main$8,L3W60WalkDeclaringClass.row
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W60WalkDeclaringClass`.
import java.util.ArrayList;
import java.util.List;
import java.util.stream.Collectors;

public class L3W60WalkDeclaringClass {
    interface Row {
        String run() throws Throwable;
    }

    static void row(String name, Row r) {
        try {
            System.out.println(name + ": " + r.run());
        } catch (Throwable t) {
            Throwable c = t.getCause() != null ? t.getCause() : t;
            System.out.println(name + ": " + c);
        }
    }

    public static String names(int n) {
        return StackWalker.getInstance().walk(s -> s.limit(n)
                .map(f -> f.getClassName() + "." + f.getMethodName())
                .collect(Collectors.joining(",")));
    }

    static String forEachNames() {
        List<String> out = new ArrayList<>();
        StackWalker.getInstance().forEach(f -> {
            if (out.isEmpty()) {
                out.add(f.getClassName() + "." + f.getMethodName());
            }
        });
        return out.get(0);
    }

    public static void main(String[] args) throws Throwable {
        row("walk-names", () -> names(3));
        row("walk-retain-declaring", () -> StackWalker
                .getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.limit(3).map(f -> f.getDeclaringClass().getName())
                        .collect(Collectors.joining(","))));
        row("walk-forEach", L3W60WalkDeclaringClass::forEachNames);
        row("walk-reflective", () -> {
            String all = (String) L3W60WalkDeclaringClass.class.getMethod("names", int.class)
                    .invoke(null, 1);
            return all + "," + (String) L3W60WalkDeclaringClass.class.getMethod("reflectTop").invoke(null);
        });
        row("walk-toString", () -> StackWalker.getInstance()
                .walk(s -> s.findFirst().map(f -> f.toStackTraceElement().getClassName() + "."
                        + f.toStackTraceElement().getMethodName()).orElse("-")));
        row("walk-names-again", () -> names(3));
    }

    public static String reflectTop() {
        return StackWalker.getInstance(StackWalker.Option.SHOW_REFLECT_FRAMES)
                .walk(s -> s.skip(1).limit(1).map(f -> f.getClassName() + "." + f.getMethodName())
                        .collect(Collectors.joining()));
    }
}
