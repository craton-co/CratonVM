// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm;

import java.lang.StackWalker;
import java.util.Set;

/**
 * Repeated deep-stack variant of Log4j2's caller-class StackWalker pattern.
 *
 * DataBufferTests hung before test logic while Netty/Log4j bootstrap repeatedly
 * resolved caller classes through StackWalker. This fixture keeps the same
 * dropWhile/findFirst/getDeclaringClass shape, but forces each walk to process a
 * deep call stack many times so regressions in transient frame caching and
 * per-walk trace storage show up as a timeout in the Rust harness.
 */
public final class StackWalkerLog4jStressProbe {
    private static final String OWNER = StackWalkerLog4jStressProbe.class.getName();
    private static final String EXPECTED = OWNER + "$LoggerFactory";
    private static final int DEPTH = 64;
    private static final int WALKS = 32;
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

    public static void main(String[] args) {
        long start = System.nanoTime();
        Class<?> caller = null;
        for (int i = 0; i < WALKS; i++) {
            caller = recurse(DEPTH);
            assertExpected(caller);
        }
        long elapsedMs = (System.nanoTime() - start) / 1_000_000L;
        System.out.println("caller=" + caller.getName());
        System.out.println("walks=" + WALKS + " depth=" + DEPTH + " elapsedMs=" + elapsedMs);
        System.out.println("STACKWALKER_LOG4J_STRESS_OK");
    }

    private static Class<?> recurse(int depth) {
        if (depth == 0) {
            return LoggerFactory.resolveCaller();
        }
        return recurse(depth - 1);
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
