// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.util.List;
import java.util.function.Supplier;
import java.util.stream.Collectors;

/**
 * SBR round 3, lane {@code ref}: {@code StackWalker.walk}, {@code StackFrameInfo} accessors and
 * {@code StackTraceElement.computeFormat} as the JDK's own bytecode. Diff against HotSpot.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr3RefStack
 * </pre>
 */
public final class Sbr3RefStack {
    static String cn(StackFrame f) {
        return f.getClassName() + "." + f.getMethodName();
    }

    static void row(String name, Supplier<Object> s) {
        try {
            System.out.println(name + " = " + s.get());
        } catch (Throwable t) {
            System.out.println(name + " threw " + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    static int depth(int n) {
        if (n == 0) {
            return StackWalker.getInstance().walk(s -> (int) s.filter(f -> f.getClassName().equals("Sbr3RefStack")
                    && f.getMethodName().equals("depth")).count());
        }
        return depth(n - 1);
    }

    interface Hook {
        List<String> run();
    }

    static List<String> viaLambda() {
        Hook h = () -> StackWalker.getInstance().walk(s -> s.limit(3).map(Sbr3RefStack::cn).collect(Collectors.toList()));
        return h.run();
    }

    public static void main(String[] args) throws Exception {
        row("default top3", () -> StackWalker.getInstance().walk(s -> s.limit(3).map(Sbr3RefStack::cn).collect(Collectors.toList())));
        row("retain top2 declaringClass", () -> StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.limit(2).map(f -> f.getDeclaringClass().getSimpleName() + ":" + f.getMethodName()).collect(Collectors.toList())));
        row("no retain getDeclaringClass", () -> StackWalker.getInstance().walk(s -> s.findFirst().get().getDeclaringClass()));
        row("toStackTraceElement", () -> StackWalker.getInstance().walk(s -> {
            StackTraceElement e = s.findFirst().get().toStackTraceElement();
            return e.getClassName() + "|" + e.getMethodName() + "|" + e.getFileName() + "|" + e.isNativeMethod()
                    + "|" + e.getModuleName() + "|" + e.getClassLoaderName();
        }));
        row("jdk frames via walker toString", () -> {
            java.util.List<String> out = new java.util.ArrayList<>();
            java.util.List.of(1).forEach(x -> StackWalker.getInstance().forEach(f -> {
                StackTraceElement e = f.toStackTraceElement();
                if (e.getClassName().startsWith("java.util")) {
                    out.add(e.toString().replaceAll(":\\d+\\)", ":N)") + " m=" + e.getModuleName() + " v="
                            + e.getModuleVersion() + " l=" + e.getClassLoaderName());
                }
            }));
            return out;
        });
        row("walker vs throwable origin", () -> {
            StackTraceElement w = StackWalker.getInstance().walk(s -> s.findFirst().get().toStackTraceElement());
            StackTraceElement t = new Throwable().getStackTrace()[0];
            return w.getClassLoaderName() + "|" + t.getClassLoaderName() + "|" + w.getModuleName() + "|" + t.getModuleName();
        });
        row("toString frame0", () -> StackWalker.getInstance().walk(s -> {
            String t = s.findFirst().get().toString();
            return t.replaceAll(":\\d+\\)", ":N)");
        }));
        row("frame fields", () -> StackWalker.getInstance().walk(s -> {
            StackFrame f = s.skip(0).findFirst().get();
            return f.getByteCodeIndex() >= 0 && f.getLineNumber() > 0 && f.getFileName().equals("Sbr3RefStack.java")
                    && f.getDescriptor().startsWith("(") && !f.isNativeMethod() && f.getMethodType() == null;
        }));
        row("method type retain", () -> StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst().get().getMethodType().toString().length() > 0));
        row("forEach count>0", () -> {
            int[] n = new int[1];
            StackWalker.getInstance().forEach(f -> n[0]++);
            return n[0] > 0;
        });
        row("getCallerClass", () -> {
            try {
                return StackWalker.getInstance(Option.RETAIN_CLASS_REFERENCE).getCallerClass().getName();
            } catch (Throwable t) {
                return t.getClass().getName();
            }
        });
        row("depth 12", () -> depth(12));
        row("lambda frames", Sbr3RefStack::viaLambda);
        row("show reflection", () -> StackWalker.getInstance(Option.SHOW_REFLECT_FRAMES).walk(s -> s.limit(1).count()));
        row("show hidden", () -> StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES).walk(s -> s.limit(1).count()));
        row("estimate 4", () -> StackWalker.getInstance(java.util.Set.of(), 4).walk(s -> s.limit(6).count()));
        row("null function", () -> StackWalker.getInstance().walk(null));
        row("stream escape", () -> {
            java.util.stream.Stream<StackFrame>[] holder = new java.util.stream.Stream[1];
            StackWalker.getInstance().walk(s -> {
                holder[0] = s;
                return null;
            });
            return holder[0].count();
        });

        // StackTraceElement / Throwable formatting
        row("ste toString", () -> {
            StackTraceElement e = new Throwable().getStackTrace()[0];
            return e.toString().replaceAll(":\\d+\\)", ":N)");
        });
        row("ste module+loader", () -> {
            StackTraceElement[] st = new Throwable().getStackTrace();
            StackTraceElement last = st[st.length - 1];
            return last.getClassName() + " " + last.getModuleName() + " " + last.getModuleVersion() + " " + last.getClassLoaderName();
        });
        row("ste jdk frame", () -> {
            try {
                Object o = java.util.List.of().get(1);
                return o;
            } catch (Throwable t) {
                StackTraceElement e = t.getStackTrace()[0];
                return e.getClassName() + " " + e.getModuleName() + " " + e.getModuleVersion() + " | " + e.toString().replaceAll(":\\d+\\)", ":N)");
            }
        });
        row("ste jdk.* module", () -> {
            try {
                java.nio.file.Files.readString(java.nio.file.Path.of("/nonexistent/zzz"));
                return "no";
            } catch (Throwable t) {
                StackTraceElement[] st = t.getStackTrace();
                StringBuilder sb = new StringBuilder();
                for (int i = 0; i < Math.min(st.length, 4); i++) {
                    sb.append(st[i].getModuleName()).append('@').append(st[i].getModuleVersion()).append(';');
                }
                return sb.toString();
            }
        });
        row("ste user 3-arg", () -> new StackTraceElement("a.B", "m", "B.java", 12).toString());
        row("ste user 7-arg", () -> new StackTraceElement("ld", "java.base", "25", "a.B", "m", "B.java", 12).toString());
        row("ste user 7-arg app", () -> new StackTraceElement("app", "mymod", "1.2", "a.B", "m", "B.java", -2).toString());
        row("ste user 7-arg unnamed", () -> new StackTraceElement(null, null, null, "a.B", "m", null, -1).toString());
        row("ste hashed jdk.compiler", () -> new StackTraceElement(null, "jdk.compiler", "25", "a.B", "m", "B.java", 3).toString());
        row("ste hashed java.compiler", () -> new StackTraceElement(null, "java.compiler", "25", "a.B", "m", "B.java", 3).toString());
        row("ste hashed java.sql", () -> new StackTraceElement(null, "java.sql", "25", "a.B", "m", "B.java", 3).toString());
        row("ste hashed jdk.zipfs", () -> new StackTraceElement("app", "jdk.zipfs", "25", "a.B", "m", "B.java", 3).toString());
        row("ste hashed jdk.httpserver", () -> new StackTraceElement("platform", "jdk.httpserver", "25", "a.B", "m", "B.java", 3).toString());
        row("ste hashed java.logging", () -> new StackTraceElement("platform", "java.logging", "25", "a.B", "m", "B.java", 3).toString());
        row("ste equals/hash", () -> {
            StackTraceElement a = new StackTraceElement("a.B", "m", "B.java", 12);
            StackTraceElement b = new StackTraceElement("a.B", "m", "B.java", 12);
            return a.equals(b) + " " + (a.hashCode() == b.hashCode());
        });
        row("printStackTrace shape", () -> {
            java.io.StringWriter sw = new java.io.StringWriter();
            new RuntimeException("x", new IllegalStateException("c")).printStackTrace(new java.io.PrintWriter(sw));
            return sw.toString().replaceAll(":\\d+\\)", ":N)").lines().filter(l -> !l.contains("Sbr3RefStack.row")
                    && !l.contains("jdk.internal") && !l.contains("java.lang.invoke") && !l.contains("lambda$")).collect(Collectors.joining("|"));
        });

        // hot, then once more: the same rows
        long sum = 0;
        for (int i = 0; i < 2000; i++) {
            sum += StackWalker.getInstance().walk(s -> s.limit(4).count());
            sum += new Throwable().getStackTrace().length > 0 ? 1 : 0;
        }
        System.out.println("hot sum=" + sum);
        row("default top3 hot", () -> StackWalker.getInstance().walk(s -> s.limit(3).map(Sbr3RefStack::cn).collect(Collectors.toList())));
        row("ste hashed jdk.compiler hot", () -> new StackTraceElement(null, "jdk.compiler", "25", "a.B", "m", "B.java", 3).toString());
        System.out.println("PROBE-DONE");
    }
}
