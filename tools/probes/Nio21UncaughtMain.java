// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.PrintStream;

/**
 * nio21 wave 8 lane L4: how a program that dies with an uncaught exception
 * ends, byte for byte against HotSpot 25 (stdout + stderr + exit code).
 *
 * <p>{@code args[0]} picks the case; each case ends the process through the
 * path it names. Compare stdout, stderr and the exit code with
 * {@code java -cp . Nio21UncaughtMain <case>}. No timing, no identity hashes.
 *
 * <ul>
 *   <li>{@code plain} -- a cause chain escapes {@code main}: the
 *       {@code Exception in thread "main"} report with no launcher prefix, rc 1;
 *   <li>{@code iae} -- an {@code InaccessibleObjectException} (the w6 shape);
 *   <li>{@code partial} -- unterminated {@code System.out.print} before the throw;
 *   <li>{@code handler} -- a default handler runs and nothing else is printed, rc 1;
 *   <li>{@code handler-exit} -- the default handler calls {@code System.exit(3)};
 *   <li>{@code handler-throws} -- the default handler throws;
 *   <li>{@code group} -- the thread's own handler (per-thread) runs;
 *   <li>{@code thread} -- a non-main thread dies; {@code main} returns, rc 0;
 *   <li>{@code thread-handler} -- the same with a default handler installed;
 *   <li>{@code error} -- an {@code Error} subclass with a message;
 *   <li>{@code soe} -- {@code StackOverflowError} (first frames only differ in depth;
 *       compare the header line and the exit code);
 *   <li>{@code tostring} -- the throwable's {@code toString()} throws;
 *   <li>{@code renamed} -- {@code main} renamed itself before throwing;
 *   <li>{@code seterr} -- {@code System.setErr(System.out)} before throwing;
 *   <li>{@code suppressed} -- a {@code Suppressed:} block;
 *   <li>{@code hook} -- a shutdown hook prints after the report;
 *   <li>{@code exit} -- {@code System.exit(7)} with no exception.
 * </ul>
 *
 * Launcher cases are separate main classes:
 * {@code Nio21UncaughtMain$NoMain} (no {@code main}: the launcher's
 * {@code Error: Main method not found} text, rc 1) and
 * {@code Nio21UncaughtMain$InstanceMain} (a non-static {@code main}, which JDK 25
 * runs: prints {@code instance-main ran}, rc 0).
 */
public class Nio21UncaughtMain {

    static final class BadToString extends RuntimeException {
        BadToString() {
            super("bad");
        }

        @Override
        public String toString() {
            throw new IllegalStateException("toString refused");
        }
    }

    static final class CustomError extends Error {
        CustomError(String m) {
            super(m);
        }
    }

    static void inner() {
        throw new IllegalStateException("inner failure");
    }

    static void outer() {
        try {
            inner();
        } catch (IllegalStateException e) {
            throw new RuntimeException("outer wrapper", e);
        }
    }

    static int depth;

    static void recurse() {
        depth++;
        recurse();
    }

    public static void main(String[] args) throws Exception {
        String sel = args.length > 0 ? args[0] : "plain";
        switch (sel) {
            case "plain":
                System.out.println("plain: before");
                outer();
                break;
            case "iae":
                throw new java.lang.reflect.InaccessibleObjectException(
                        "Unable to make field private final byte[] java.lang.String.value accessible");
            case "partial":
                System.out.print("partial: no newline");
                throw new IllegalArgumentException("after partial");
            case "handler":
                Thread.setDefaultUncaughtExceptionHandler(
                        (t, e) -> System.out.println("handler: " + t.getName() + " " + e));
                throw new IllegalStateException("to the handler");
            case "handler-exit":
                Thread.setDefaultUncaughtExceptionHandler((t, e) -> {
                    System.out.println("handler-exit: " + e.getMessage());
                    System.exit(3);
                });
                throw new IllegalStateException("exit from handler");
            case "handler-throws":
                Thread.setDefaultUncaughtExceptionHandler((t, e) -> {
                    System.out.println("handler-throws: " + e.getMessage());
                    throw new UnsupportedOperationException("handler failed");
                });
                throw new IllegalStateException("handler will throw");
            case "group":
                Thread.currentThread().setUncaughtExceptionHandler(
                        (t, e) -> System.out.println("own-handler: " + t.getName() + " " + e.getMessage()));
                throw new IllegalStateException("per-thread handler");
            case "thread": {
                Thread w = new Thread(() -> {
                    throw new IllegalStateException("worker died");
                }, "worker-1");
                w.start();
                w.join();
                System.out.println("thread: main returns");
                break;
            }
            case "thread-handler": {
                Thread.setDefaultUncaughtExceptionHandler(
                        (t, e) -> System.out.println("default-handler: " + t.getName() + " " + e.getMessage()));
                Thread w = new Thread(() -> {
                    throw new IllegalStateException("worker died");
                }, "worker-2");
                w.start();
                w.join();
                System.out.println("thread-handler: main returns");
                break;
            }
            case "error":
                throw new CustomError("custom error escapes");
            case "soe":
                try {
                    recurse();
                } finally {
                    System.out.println("soe: depth>1000=" + (depth > 1000));
                }
                break;
            case "tostring":
                System.out.println("tostring: before");
                throw new BadToString();
            case "renamed":
                Thread.currentThread().setName("renamed-main");
                throw new IllegalStateException("from a renamed main");
            case "seterr": {
                PrintStream out = System.out;
                System.setErr(out);
                throw new IllegalStateException("report goes to stdout");
            }
            case "suppressed": {
                IllegalStateException e = new IllegalStateException("primary");
                e.addSuppressed(new IllegalArgumentException("secondary"));
                throw e;
            }
            case "hook":
                Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook: ran")));
                throw new IllegalStateException("before the hook");
            case "exit":
                System.out.println("exit: 7");
                System.exit(7);
                break;
            default:
                System.out.println("unknown case " + sel);
        }
    }

    /** No {@code main} at all: the launcher refuses it. */
    public static class NoMain {
        public static void notMain(String[] args) {
        }
    }

    /** A non-static {@code main}: JDK 25 instantiates the class and runs it. */
    public static class InstanceMain {
        public void main(String[] args) {
            System.out.println("instance-main ran");
        }
    }
}
