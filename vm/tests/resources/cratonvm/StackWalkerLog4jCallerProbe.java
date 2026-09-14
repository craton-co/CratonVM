// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm;

import java.lang.StackWalker;
import java.util.Set;

/**
 * Log4j2-shaped StackWalker caller-class probe.
 *
 * Log4j2's Java 9+ StackLocator anchors on its own class, drops its own frames,
 * then picks the first frame in the requested package and maps it through
 * StackFrame.getDeclaringClass(). CratonVM must return a real caller class from
 * that pipeline; returning null or a walker-internal class can make logging
 * context creation recurse during framework bootstrap.
 */
public final class StackWalkerLog4jCallerProbe {
    private static final String OWNER = StackWalkerLog4jCallerProbe.class.getName();
    private static final String EXPECTED = OWNER + "$LoggerFactory";
    private static final StackWalker WALKER =
            StackWalker.getInstance(Set.of(StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static final class Locator {
        static Class<?> getCallerClass(String fqcn, String pkg) {
            return WALKER.walk(stream -> stream
                    .dropWhile(frame -> !frame.getClassName().equals(fqcn))
                    .dropWhile(frame -> frame.getClassName().equals(fqcn))
                    .dropWhile(frame -> !frame.getClassName().startsWith(pkg))
                    .findFirst()
                    .map(StackWalker.StackFrame::getDeclaringClass)
                    .orElse(null));
        }
    }

    static final class LoggerFactory {
        static Class<?> resolveCaller() {
            return Locator.getCallerClass(Locator.class.getName(), OWNER);
        }
    }

    static final class AllocatorInit {
        static final Class<?> CALLER = LoggerFactory.resolveCaller();
    }

    public static void main(String[] args) {
        Class<?> caller = null;
        for (int i = 0; i < 16; i++) {
            caller = LoggerFactory.resolveCaller();
            assertExpected(caller);
        }
        assertExpected(AllocatorInit.CALLER);
        System.out.println("caller=" + caller.getName());
        System.out.println("STACKWALKER_LOG4J_CALLER_OK");
    }

    private static void assertExpected(Class<?> caller) {
        if (caller == null) {
            throw new AssertionError("StackWalker returned null caller");
        }
        if (!EXPECTED.equals(caller.getName())) {
            throw new AssertionError("expected " + EXPECTED + " but got " + caller.getName());
        }
    }
}
