// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

import java.util.Collections;
import java.util.Iterator;

/**
 * Focused, framework-independent fixture for the "nested concrete-class
 * {@code <clinit>} under partial bootstrap" gap (real-cdi-bean-container,
 * increment 2 / Step 2).
 *
 * <p>It mirrors the exact shape of Spring's
 * {@code org.springframework.core.metrics.ApplicationStartup} /
 * {@code DefaultApplicationStartup} / {@code DefaultStartupStep} /
 * {@code DefaultTags} classes that the {@code spring_startup_bootstrap.rs} shim
 * papers over — but without pulling in Spring:
 *
 * <ul>
 *   <li>{@link Startup} is an interface with a NON-constant {@code static final}
 *       field {@code DEFAULT}, assigned in the interface {@code <clinit>} by
 *       {@code new DefaultStartup()} (mirrors {@code ApplicationStartup.DEFAULT}).</li>
 *   <li>{@link DefaultStartup} is a concrete impl whose {@code <clinit>} creates
 *       a {@code static final} {@code DefaultStep} singleton (mirrors
 *       {@code DefaultApplicationStartup.DEFAULT_STARTUP_STEP}). This is the
 *       NESTED concrete-class {@code <clinit>} that used to NPE / be swallowed
 *       and backfilled by {@code post_clinit_fixup}.</li>
 *   <li>{@link DefaultStep} holds a {@code final} {@code DefaultTags} instance
 *       built in its {@code <init>} (mirrors {@code DefaultStartupStep.TAGS}).</li>
 *   <li>{@link DefaultTags#iterator()} returns {@code Collections.emptyIterator()}
 *       exactly like Spring's {@code DefaultTags}.</li>
 * </ul>
 *
 * <p>The bytecode chain is deliberately trivial (no I/O, reflection or resource
 * loading), matching the real Spring classes: the only reason it failed under
 * CratonVM's partial bootstrap was that the no-op startup-metrics shim shadowed
 * the real getter and the nested {@code <clinit>} was swallowed + backfilled.
 * Under {@code CRATONVM_REAL_SPRING_STARTUP} the real chain must complete.
 */
public class NestedClinitStartup {

    interface Startup {
        // NON-constant static-final on an interface: assigned by Startup.<clinit>
        // via `new DefaultStartup()`, which in turn triggers DefaultStartup.<clinit>.
        Startup DEFAULT = new DefaultStartup();

        Step start(String name);
    }

    interface Step {
        String getName();

        Tags getTags();

        interface Tags extends Iterable<String> {
        }
    }

    static class DefaultStartup implements Startup {
        // Concrete-class static-final singleton built in DefaultStartup.<clinit>.
        private static final DefaultStep DEFAULT_STEP = new DefaultStep();

        DefaultStartup() {
        }

        @Override
        public DefaultStep start(String name) {
            return DEFAULT_STEP;
        }
    }

    static class DefaultStep implements Step {
        private final DefaultTags tags;

        DefaultStep() {
            this.tags = new DefaultTags();
        }

        @Override
        public String getName() {
            return "default";
        }

        @Override
        public DefaultTags getTags() {
            return this.tags;
        }
    }

    static class DefaultTags implements Step.Tags {
        @Override
        public Iterator<String> iterator() {
            return Collections.emptyIterator();
        }
    }

    /**
     * Drives the full nested {@code <clinit>} chain and exercises the live
     * objects it produced. A single {@code getstatic Startup.DEFAULT} triggers
     * {@code Startup.<clinit>} → {@code new DefaultStartup} →
     * {@code DefaultStartup.<clinit>} → {@code new DefaultStep} →
     * {@code new DefaultTags}.
     *
     * <p>Returns {@code true} iff the whole chain ran and the singleton step is
     * usable: {@code DEFAULT} is non-null, {@code start("x")} returns a non-null
     * step, {@code getName()} == {@code "default"}, and {@code getTags()} yields
     * a non-null, empty {@code Iterable}. Any swallowed {@code <clinit>} would
     * leave {@code DEFAULT} null (or its step half-built) and flip this to
     * {@code false} / throw.
     */
    public static boolean probeNestedStartupClinit() {
        Startup s = Startup.DEFAULT;
        if (s == null) {
            return false;
        }
        Step step = s.start("probe");
        if (step == null) {
            return false;
        }
        if (!"default".equals(step.getName())) {
            return false;
        }
        Step.Tags tags = step.getTags();
        if (tags == null) {
            return false;
        }
        // Empty, but a real iterable — iterating must not throw.
        int count = 0;
        for (String ignored : tags) {
            count++;
        }
        return count == 0;
    }

    /**
     * Subprocess entry point. Prints a single marker line that the
     * {@code nested_clinit_startup} integration test asserts on:
     * {@code NESTED_STARTUP_OK} on success.
     */
    public static void main(String[] args) {
        boolean ok = probeNestedStartupClinit();
        System.out.println("probe=" + ok);
        if (ok) {
            System.out.println("NESTED_STARTUP_OK");
        } else {
            System.out.println("NESTED_STARTUP_FAIL");
        }
    }
}
