// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L3 (hunt): HotSpot's `JVM_GetCallerClass`
// requires the method that asks `jdk.internal.reflect.Reflection.getCallerClass()`
// (frame 1) to be `@CallerSensitive`, and honours that annotation only in a
// class the boot or platform loader defines. Application code that reaches
// `Reflection` (with `--add-exports java.base/jdk.internal.reflect`) gets
// `InternalError: CallerSensitive annotation expected at frame 1` -- annotated
// or not, interpreted or compiled (C2's intrinsic bails out for such a caller).
// CratonVM answered the asker's caller.
//
// Rows:
//   direct        -- a plain static method asks;
//   annotated     -- the same, annotated `@CallerSensitive` (ignored outside the JDK);
//   in-lambda     -- a lambda body asks;
//   hot           -- `direct` 20,000 times, count of InternalErrors (compiled);
//   jdk-control   -- `Class.forName` (a JDK `@CallerSensitive` method) still works.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from jvm.cpp's JVM_GetCallerClass)
// direct: java.lang.InternalError CallerSensitive annotation expected at frame 1
// annotated: java.lang.InternalError CallerSensitive annotation expected at frame 1
// in-lambda: java.lang.InternalError CallerSensitive annotation expected at frame 1
// hot: 20000/20000
// jdk-control: java.lang.String
// HOTSPOT_EXPECTED_END
//
// SETUP: `javac --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED L3W106CallerClassAsker.java`,
// then `java|cratonvm [--compatible] [--nojit] --add-exports java.base/jdk.internal.reflect=ALL-UNNAMED
// -cp . L3W106CallerClassAsker`.
import java.util.function.Supplier;
import jdk.internal.reflect.CallerSensitive;
import jdk.internal.reflect.Reflection;

public class L3W106CallerClassAsker {
    static Class<?> direct() {
        return Reflection.getCallerClass();
    }

    @CallerSensitive
    static Class<?> annotated() {
        return Reflection.getCallerClass();
    }

    static String row(Supplier<Class<?>> s) {
        try {
            Class<?> c = s.get();
            return "no exception: " + (c == null ? "null" : c.getName());
        } catch (Throwable t) {
            return t.getClass().getName() + " " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Throwable {
        System.out.println("direct: " + row(L3W106CallerClassAsker::direct));
        System.out.println("annotated: " + row(L3W106CallerClassAsker::annotated));
        System.out.println("in-lambda: " + row(() -> Reflection.getCallerClass()));
        int errors = 0;
        final int n = 20_000;
        for (int i = 0; i < n; i++) {
            try {
                direct();
            } catch (InternalError e) {
                errors++;
            }
        }
        System.out.println("hot: " + errors + "/" + n);
        System.out.println("jdk-control: " + Class.forName("java.lang.String").getName());
    }
}
