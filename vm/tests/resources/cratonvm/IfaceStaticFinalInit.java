// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Focused fixture for the "non-constant static-final field on an INTERFACE is
 * initialized when first accessed" regression (real-cdi-bean-container,
 * increment 1).
 *
 * <p>Per JVMS §5.5 / §5.4.3.2, a {@code static final} field on an interface
 * whose initializer is NOT a compile-time constant does NOT get a
 * {@code ConstantValue} attribute. Its value is assigned by the interface's
 * {@code <clinit>}, which the VM must run lazily on the first {@code getstatic}
 * that reads the field. If the interface {@code <clinit>} is skipped, the field
 * reads back as its prepared default ({@code 0} for {@code int}) instead of the
 * computed value.
 *
 * <p>This fixture is intentionally Spring-independent: it reproduces the same
 * VM gap that the {@code ApplicationStartup.DEFAULT} (a non-constant
 * static-final on the {@code ApplicationStartup} interface) shim used to paper
 * over, without pulling in any framework.
 */
public class IfaceStaticFinalInit {

    /**
     * Interface declaring a NON-constant static-final field. {@code compute()}
     * is a method call, so {@code VALUE} cannot be a compile-time constant and
     * carries no {@code ConstantValue} attribute — the interface {@code <clinit>}
     * must run to assign it.
     */
    interface IntHolder {
        int VALUE = compute();

        static int compute() {
            return 7 * 6; // 42 — computed at runtime, never inlined as a constant
        }
    }

    /**
     * Reads the interface's non-constant static-final field. The single
     * {@code getstatic IntHolder.VALUE} here must trigger {@code IntHolder.<clinit>}
     * on first access. Returns 42 iff the interface was initialized; returns the
     * prepared default 0 if the interface {@code <clinit>} was wrongly skipped.
     */
    public static int probeInterfaceStaticFinal() {
        return IntHolder.VALUE;
    }
}
