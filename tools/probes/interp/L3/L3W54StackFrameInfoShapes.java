// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L3 -- the shapes of a JDK stack walk's
// frames (`--jdk-only` runs the JDK's own `StackFrameInfo` / `ClassFrameInfo`
// bodies over the frames the VM's walk fills):
//
//   * a `DROP_METHOD_INFO` walker's frames are `ClassFrameInfo`s: the method
//     accessors throw `UnsupportedOperationException`, `toString()` is the
//     class name and a space. The VM's walk replaced the JDK's
//     `ClassFrameInfo` buffer elements with `StackFrameInfo`s, so every
//     accessor answered (fixed in wave 54: `lang_stackwalker::fill_class_frame_info`);
//   * a native frame (`Class.forName0`, under a static initializer that
//     `Class.forName` runs) answers `isNativeMethod() == true`, bci `-1`,
//     line `-2`: `populate_sfi` wrote no `Modifier.NATIVE` bit in `flags`;
//   * a walked frame's `toString()` names the module of a JDK frame
//     (`java.base/java.lang.Class.forName0(Native Method)`): the element the
//     walk pre-cached carried no module, and `computeFormat` never ran.
//
// Rows (the first is repeated as the last, for a cold-VM gap):
//   drop-*        -- each accessor of the first frame of a DROP_METHOD_INFO
//                    walk (and with RETAIN_CLASS_REFERENCE); `drop-hidden`,
//                    a SHOW_HIDDEN_FRAMES one through a lambda, whose proxy
//                    frame `toString()` tags ` hidden`;
//   native-*      -- the `Class.forName0` frame of a walk made from a static
//                    initializer `Class.forName` runs;
//   plain-string  -- `toString()` of a plain walk's first frame.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// drop-first-string: [L3W54StackFrameInfoShapes ]
// drop-class: L3W54StackFrameInfoShapes
// drop-string: [L3W54StackFrameInfoShapes ]
// drop-method: java.lang.UnsupportedOperationException
// drop-bci: java.lang.UnsupportedOperationException
// drop-file: java.lang.UnsupportedOperationException
// drop-line: java.lang.UnsupportedOperationException
// drop-native: java.lang.UnsupportedOperationException
// drop-element: java.lang.UnsupportedOperationException
// drop-declaring: java.lang.UnsupportedOperationException: No access to RETAIN_CLASS_REFERENCE
// drop-retain-class: L3W54StackFrameInfoShapes
// drop-retain-string: [L3W54StackFrameInfoShapes ]
// drop-retain-method: java.lang.UnsupportedOperationException
// drop-retain-bci: java.lang.UnsupportedOperationException
// drop-retain-file: java.lang.UnsupportedOperationException
// drop-retain-line: java.lang.UnsupportedOperationException
// drop-retain-native: java.lang.UnsupportedOperationException
// drop-retain-element: java.lang.UnsupportedOperationException
// drop-retain-declaring: L3W54StackFrameInfoShapes
// native-native: true
// native-bci: -1
// native-line: -2
// native-file: Class.java
// native-string: java.base/java.lang.Class.forName0(Native Method)
// native-element-native: true
// drop-hidden: [[L3W54StackFrameInfoShapes ], [L3W54StackFrameInfoShapes$$Lambda/0x  hidden], [L3W54StackFrameInfoShapes ]]
// plain-string: L3W54StackFrameInfoShapes.main(L3W54StackFrameInfoShapes.java:line)
// plain-native: false
// drop-first-string-again: [L3W54StackFrameInfoShapes ]
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 54, `--jdk-only` (read from the code): every `drop-*`
// accessor answered the frame's method details and `drop-*-string` was the
// element's text; `native-native: false`, `native-bci` the raw index, and
// `native-string` without its module (`java.lang.Class.forName0(Native Method)`).
// `--compatible`'s carrier answered the `drop-*` rows that way until wave 55
// (lane L3) built the JDK's classes there too:
// docs/internal/fixed-bugs/interpreter-L3-the-compatible-walk-carrier-is-not-the-jdks-frame-classes-FIXED-20261019.md.
// The `native-*` rows need the walk to list `Class.forName0`; a walk that
// lists no such frame prints `native: no forName0 frame`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W54StackFrameInfoShapes`.
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.concurrent.Callable;

public class L3W54StackFrameInfoShapes {
    static final List<StackWalker.StackFrame> FROM_INIT = new ArrayList<>();

    /** Initialized by `Class.forName`, under `Class.forName0`. */
    public static class Init {
        static {
            StackWalker.getInstance().forEach(FROM_INIT::add);
        }
    }

    static String call(Callable<Object> c) {
        try {
            Object v = c.call();
            return String.valueOf(v);
        } catch (Throwable t) {
            return t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
    }

    static String lineOf(int n) {
        return n > 0 ? "line" : Integer.toString(n);
    }

    static void drop(String name, StackWalker.StackFrame f) {
        System.out.println(name + "-class: " + call(f::getClassName));
        System.out.println(name + "-string: [" + call(f::toString) + "]");
        System.out.println(name + "-method: " + call(f::getMethodName));
        System.out.println(name + "-bci: " + call(f::getByteCodeIndex));
        System.out.println(name + "-file: " + call(f::getFileName));
        System.out.println(name + "-line: " + call(f::getLineNumber));
        System.out.println(name + "-native: " + call(f::isNativeMethod));
        System.out.println(name + "-element: " + call(() -> f.toStackTraceElement().getMethodName()));
        System.out.println(name + "-declaring: " + call(() -> f.getDeclaringClass().getSimpleName()));
    }

    public static void main(String[] args) throws Exception {
        StackWalker.StackFrame d = StackWalker.getInstance(StackWalker.Option.DROP_METHOD_INFO)
                .walk(s -> s.findFirst().get());
        System.out.println("drop-first-string: [" + call(d::toString) + "]");
        drop("drop", d);
        StackWalker.StackFrame dr = StackWalker.getInstance(
                        Set.of(StackWalker.Option.DROP_METHOD_INFO, StackWalker.Option.RETAIN_CLASS_REFERENCE))
                .walk(s -> s.findFirst().get());
        drop("drop-retain", dr);
        Class.forName(L3W54StackFrameInfoShapes.class.getName() + "$Init");
        StackWalker.StackFrame nat = null;
        for (StackWalker.StackFrame f : FROM_INIT) {
            if (f.getMethodName().equals("forName0")) {
                nat = f;
                break;
            }
        }
        if (nat == null) {
            System.out.println("native: no forName0 frame");
        } else {
            System.out.println("native-native: " + nat.isNativeMethod());
            System.out.println("native-bci: " + nat.getByteCodeIndex());
            System.out.println("native-line: " + lineOf(nat.getLineNumber()));
            System.out.println("native-file: " + nat.getFileName());
            System.out.println("native-string: " + nat);
            System.out.println("native-element-native: " + nat.toStackTraceElement().isNativeMethod());
        }
        // A lambda's proxy frame, listed by SHOW_HIDDEN_FRAMES: `ClassFrameInfo`
        // tags it ` hidden` (the proxy's address normalised).
        List<String> hiddenRows = new ArrayList<>();
        Runnable viaLambda = () -> StackWalker.getInstance(
                        Set.of(StackWalker.Option.DROP_METHOD_INFO, StackWalker.Option.SHOW_HIDDEN_FRAMES))
                .walk(s -> s.limit(3).toList())
                .forEach(f -> hiddenRows.add("[" + f.toString().replaceAll("/0x[0-9a-f]+", "/0x") + "]"));
        viaLambda.run();
        System.out.println("drop-hidden: " + hiddenRows);
        StackWalker.StackFrame p = StackWalker.getInstance().walk(s -> s.findFirst().get());
        System.out.println("plain-string: " + p.toString().replaceAll(":\\d+\\)", ":line)"));
        System.out.println("plain-native: " + p.isNativeMethod());
        StackWalker.StackFrame d2 = StackWalker.getInstance(StackWalker.Option.DROP_METHOD_INFO)
                .walk(s -> s.findFirst().get());
        System.out.println("drop-first-string-again: [" + call(d2::toString) + "]");
    }
}
