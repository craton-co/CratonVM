// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.Console;

/**
 * Wave 3 probe: {@code System.console()} on a non-TTY must not blow up inside
 * {@code Console.instantiateConsole()}.
 *
 * <p>Driven by {@code vm/tests/wave3_console_module.rs}. Bootstrapping a
 * {@link Console} runs {@code ServiceLoader.load(JdkConsoleProvider.class)},
 * whose {@code checkCaller} asks the caller's module {@code canUse(service)}.
 * The real {@code Module.canUse} bytecode dereferences {@code this.descriptor},
 * a field our synthetic 2-field Module shape does not have, so without the
 * registered native override that call NPEs deep inside the service lookup and
 * this probe dies before printing anything.
 *
 * <p>Nothing here prints an expected literal: the {@code console=} line reports
 * whatever {@link System#console()} actually handed back (its concrete class
 * name when non-null), and {@code OK} is only reached if no exception escaped.
 *
 * <p>The two {@code Module.canUse} checks pin the spec-correct edge cases of
 * the override directly. They are made against THIS class's own module, which
 * is the unnamed (classpath) module — {@code canUse} short-circuits to
 * {@code true} there on HotSpot, and the override also answers {@code true}, so
 * the check is a genuine agreement test rather than a restatement of the
 * override. A named module is deliberately not used: HotSpot would answer from
 * a real module descriptor while the permissive override answers {@code true},
 * and the disagreement would be a false red.
 */
public final class ConsoleProbe {

    public static void main(String[] args) {
        Module self = ConsoleProbe.class.getModule();

        // Unnamed (classpath) module: every service is usable.
        boolean canUse = self.canUse(Console.class);
        if (!canUse) {
            throw new AssertionError(
                    "Module.canUse(Console.class) returned false for the unnamed module "
                            + self + "; expected true");
        }

        // Null service class is a NullPointerException, not `false`.
        boolean threwNpe = false;
        try {
            self.canUse(null);
        } catch (NullPointerException expected) {
            threwNpe = true;
        }
        if (!threwNpe) {
            throw new AssertionError("Module.canUse(null) must throw NullPointerException");
        }

        // The load-bearing call: this is what drags ServiceLoader.checkCaller,
        // and therefore Module.canUse, into the run.
        Console console = System.console();
        System.out.println("console=" + (console == null ? "null" : console.getClass().getName()));
        System.out.println("OK");
    }
}
