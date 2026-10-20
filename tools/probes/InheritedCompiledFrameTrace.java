// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

/**
 * A compiled INHERITED method must report its declaring class in a stack trace.
 *
 * <p>The by-name callee door publishes an inherited method's body under its
 * receiver class's key; before i74-L2 (2026-10-11) the compiled frame then named
 * the receiver class too, so its source file was the subclass's and its line
 * was lost: under {@code --jdk-only} {@code BuiltinClassLoader.loadClass} read
 * {@code (ClassLoaders.java)} once it compiled. Two rows:
 *
 * <ul>
 *   <li>{@code user}: {@code InheritedCompiledFrameTraceSub} inherits
 *       {@code make} from {@code InheritedCompiledFrameTraceBase} (a separate
 *       source file name each, through the {@code SourceFile} each nested class
 *       carries: the same file, so the line is the witness here);
 *   <li>{@code platform}: a delegation miss through the platform loader, the
 *       shape {@code L2W73ThrowableTraceShapes two-sites-stable} found.
 * </ul>
 *
 * <p>Each row repeats one throw site and prints {@code stable true} when every
 * repetition's trace equals the first, then the first and the last trace.
 * HotSpot and {@code --nojit} print {@code stable true} for both rows.
 */
public class InheritedCompiledFrameTrace {
    static class Base {
        Exception make(int i) {
            return new Exception("e" + (i & 1));
        }
    }

    static class Sub extends Base {
    }

    static Exception platformMiss(ClassLoader loader) {
        try {
            loader.loadClass("inherited.Missing");
            return null;
        } catch (ClassNotFoundException e) {
            return e;
        }
    }

    static String render(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            sb.append(' ').append(e.getClassName()).append('.').append(e.getMethodName())
                .append('(').append(e.getFileName()).append(':').append(e.getLineNumber()).append(')');
        }
        return sb.toString();
    }

    static void report(String name, boolean stable, StackTraceElement[] first, StackTraceElement[] last) {
        System.out.println(name + " stable " + stable);
        System.out.println(name + " first" + render(first));
        System.out.println(name + " last" + render(last));
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 3000;
        Base sub = new Sub();
        StackTraceElement[] first = null;
        StackTraceElement[] last = null;
        boolean stable = true;
        for (int i = 0; i < n; i++) {
            StackTraceElement[] st = sub.make(i).getStackTrace();
            if (first == null) {
                first = st;
            } else {
                stable &= Arrays.equals(first, st);
            }
            last = st;
        }
        report("user", stable, first, last);

        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        first = null;
        stable = true;
        for (int i = 0; i < n / 5; i++) {
            StackTraceElement[] st = platformMiss(platform).getStackTrace();
            if (first == null) {
                first = st;
            } else {
                stable &= Arrays.equals(first, st);
            }
            last = st;
        }
        report("platform", stable, first, last);
    }
}
