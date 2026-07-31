/*
 * SPDX-License-Identifier: Apache-2.0
 * Copyright 2024-2026 Craton Software Company
 */

import java.lang.reflect.InaccessibleObjectException;
import java.lang.reflect.Method;
import java.security.ProtectionDomain;

/**
 * Witness: {@code java.base} does not {@code opens java.lang}, so classpath
 * (unnamed-module) code must NOT be able to {@code setAccessible(true)} the
 * protected {@code ClassLoader.defineClass}.
 *
 * <p>This is not a curiosity. Spring CGLIB's {@code ReflectUtils.defineClass}
 * tries, in order:
 *
 * <ol>
 *   <li>{@code Lookup.defineClass} when the requested loader equals the
 *       neighbour class's loader;</li>
 *   <li>the reflective {@code ClassLoader.defineClass};</li>
 *   <li>{@code Lookup.defineClass} on the neighbour class even though the
 *       loaders differ.</li>
 * </ol>
 *
 * <p>On HotSpot option 2 is unavailable without
 * {@code --add-opens java.base/java.lang=ALL-UNNAMED}, so a CGLIB proxy for a
 * class owned by another loader lands in <em>its superclass's</em> loader — the
 * same runtime package — and its package-private overrides genuinely override.
 * A VM that allows option 2 puts the proxy in the requested loader instead;
 * JVMS 5.4.5 then says the package-private method does not override, and
 * {@code invokevirtual} on the proxy silently runs the superclass body — no
 * interceptor, no advice. That is what made {@code @MockitoSpyBean} stubs
 * disappear behind a Spring AOP proxy in Spring's AOT replay
 * ({@code MockitoSpyBeanAndSpringAopProxyIntegrationTests}, 4/4 fail).
 *
 * <p>Run with no {@code --add-opens}. Expected on both HotSpot and CratonVM:
 * {@code DEFINECLASSACCESS DENIED}.
 */
public class DefineClassAccessProbe {

    public static void main(String[] args) throws Exception {
        Method m = ClassLoader.class.getDeclaredMethod("defineClass",
                String.class, byte[].class, int.class, int.class, ProtectionDomain.class);
        boolean denied;
        try {
            m.setAccessible(true);
            denied = false;
            System.out.println("setAccessible(ClassLoader.defineClass) = ALLOWED");
        }
        catch (InaccessibleObjectException | SecurityException ex) {
            denied = true;
            System.out.println("setAccessible(ClassLoader.defineClass) threw "
                    + ex.getClass().getName() + ": " + ex.getMessage());
        }
        System.out.println("trySetAccessible() = " + m.trySetAccessible());
        System.out.println("DEFINECLASSACCESS " + (denied ? "DENIED" : "ALLOWED"));
        if (!denied) {
            throw new AssertionError(
                    "ClassLoader.defineClass must stay encapsulated without --add-opens");
        }
    }
}
