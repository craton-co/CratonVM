// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L7 (filed for lane L4's lambda area): a
// lambda whose instantiated parameter is `String`, called through a raw
// `Function` with ANOTHER LAMBDA as the argument. HotSpot's lambda class
// casts the argument in its bridge and throws `ClassCastException`; CratonVM's
// argument check (`interpreter/lambda.rs`, `lambda_arg_cast_verdict`) fails
// open for a lambda-proxy argument (no class-store entry), so the body runs on
// the wrong object and dies with `NoSuchMethodError: 'int
// java.lang.Runnable.length()'`. A `String`-typed argument of another class
// (row `integer-arg`) is checked correctly. `/0x<hex>` is printed as `/0x?`.
// Page: docs/internal/fixed-bugs/interpreter-L4-a-lambda-argument-that-is-a-lambda-skips-the-bridge-cast-FIXED-20261014.md
// (fixed in wave 50, lane L4).
//
// Expected (HotSpot 25.0.3, `java L7W49LambdaArgCheck`, and `-Xint`):
//   r0 lambda-arg ClassCastException: class L7W49LambdaArgCheck$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaArgCheck$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r0 integer-arg ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
//   r1 lambda-arg ClassCastException: class L7W49LambdaArgCheck$$Lambda/0x? cannot be cast to class java.lang.String (L7W49LambdaArgCheck$$Lambda/0x? is in unnamed module of loader 'app'; java.lang.String is in module java.base of loader 'bootstrap')
//   r1 integer-arg ClassCastException: class java.lang.Integer cannot be cast to class java.lang.String (java.lang.Integer and java.lang.String are in module java.base of loader 'bootstrap')
import java.util.function.Function;

public class L7W49LambdaArgCheck {
    static Object id(Object o) {
        return o;
    }

    @SuppressWarnings({"unchecked", "rawtypes"})
    static void row(String label, Object arg) {
        Function raw = (Function<String, Integer>) s -> s.length();
        try {
            raw.apply(arg);
            System.out.println(label + " no exception");
        } catch (Throwable t) {
            System.out.println(label + " " + t.getClass().getSimpleName() + ": "
                    + String.valueOf(t.getMessage()).replaceAll("/0x[0-9a-fA-F]+", "/0x?"));
        }
    }

    public static void main(String[] args) {
        for (int r = 0; r < 2; r++) {
            row("r" + r + " lambda-arg", id((Runnable) () -> { }));
            row("r" + r + " integer-arg", id(Integer.valueOf(7)));
        }
    }
}
