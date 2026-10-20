// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 51, lane L3 (review) -- the frames of a
// `NoSuchFieldException` / `NoSuchMethodException` / `ClassNotFoundException`
// that a `java.lang.Class` member-lookup method raises. HotSpot runs the JDK
// methods, so the throwable's trace starts at the JDK method that threw
// (`Class.getField`, `Class.getMethod`, ...); this VM serves several of them
// with Bridges that raise the throwable from Rust, and the trace starts at
// the caller. Each row prints the first two frames as `class.method`
// (no line numbers: they differ between JDK builds) and the trace's depth
// below the probe's own `ask` frame.
//
// Rows (the first is repeated as the last):
//   getField             -- `getField("NOPE")`;
//   getDeclaredField     -- `getDeclaredField("NOPE")`;
//   getMethod            -- `getMethod("nope")`;
//   getDeclaredMethod    -- `getDeclaredMethod("nope")`;
//   getConstructor       -- `getConstructor(int.class)`;
//   forName              -- `Class.forName("NopeL3W51")`;
//   getField-again       -- `getField` again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// getField: java.lang.NoSuchFieldException java.lang.Class.getField < L3W51MemberLookupThrowFrames.lambda$main$0 jdk-frames=1
// getDeclaredField: java.lang.NoSuchFieldException java.lang.Class.getDeclaredField < L3W51MemberLookupThrowFrames.lambda$main$1 jdk-frames=1
// getMethod: java.lang.NoSuchMethodException java.lang.Class.getMethod < L3W51MemberLookupThrowFrames.lambda$main$2 jdk-frames=1
// getDeclaredMethod: java.lang.NoSuchMethodException java.lang.Class.getDeclaredMethod < L3W51MemberLookupThrowFrames.lambda$main$3 jdk-frames=1
// getConstructor: java.lang.NoSuchMethodException java.lang.Class.getConstructor0 < java.lang.Class.getConstructor jdk-frames=2
// forName: java.lang.ClassNotFoundException jdk.internal.loader.BuiltinClassLoader.loadClass < java.lang.ClassLoader.loadClass jdk-frames=5
// getField-again: java.lang.NoSuchFieldException java.lang.Class.getField < L3W51MemberLookupThrowFrames.lambda$main$6 jdk-frames=1
// HOTSPOT_EXPECTED_END
//
// CratonVM at the wave-51 base (measured, Windows): every row but
// `getDeclaredMethod` under `--jdk-only` (whose Bridge is retired there, so
// the JDK method runs) starts at the probe's lambda with `jdk-frames=0`; under
// `--compatible` all seven do. Since wave 52 (lane L3,
// `CRATONVM_THROWABLE_MEMBER_LOOKUP_FRAMES`) every row matches in all four
// modes (Windows; `forName` already matched locally at the wave-52 base). See
// docs/internal/fixed-bugs/interpreter-L3-a-class-member-lookup-bridge-raises-without-the-jdk-frame-FIXED-20261016.md.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W51MemberLookupThrowFrames`.
public class L3W51MemberLookupThrowFrames {
    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            call.run();
            out = "no exception";
        } catch (Throwable t) {
            StackTraceElement[] st = t.getStackTrace();
            int jdk = 0;
            while (jdk < st.length && !st[jdk].getClassName().startsWith("L3W51")) {
                jdk++;
            }
            out = t.getClass().getName() + " " + frame(st, 0) + " < " + frame(st, 1) + " jdk-frames=" + jdk;
        }
        System.out.println(name + ": " + out);
    }

    static String frame(StackTraceElement[] st, int i) {
        return i < st.length ? st[i].getClassName() + "." + st[i].getMethodName() : "-";
    }

    public static void main(String[] args) {
        Class<?> c = L3W51MemberLookupThrowFrames.class;
        row("getField", () -> c.getField("NOPE"));
        row("getDeclaredField", () -> c.getDeclaredField("NOPE"));
        row("getMethod", () -> c.getMethod("nope"));
        row("getDeclaredMethod", () -> c.getDeclaredMethod("nope"));
        row("getConstructor", () -> c.getConstructor(int.class));
        row("forName", () -> Class.forName("NopeL3W51"));
        row("getField-again", () -> c.getField("NOPE"));
    }
}
