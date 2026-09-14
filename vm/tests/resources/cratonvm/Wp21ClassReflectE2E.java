package cratonvm;

import java.lang.reflect.Method;
import java.sql.Connection;
import java.sql.ResultSet;

/**
 * WP2.1-narrow — end-to-end probe that {@code Class.getDeclaredMethods()}
 * dispatches from real bytecode through the synthetic-class method
 * declaration layer to a non-empty {@code Method[]} whose entries have
 * valid {@code getName()} and {@code toString()} results.
 *
 * <h2>What this fixture proves</h2>
 * <ol>
 *   <li>The {@code invokevirtual java/lang/Class.getDeclaredMethods}
 *       site emitted by javac resolves end-to-end: bytecode dispatch
 *       finds the registered native and the native runs.</li>
 *   <li>The synthetic-stub method-declaration table in
 *       {@code native-builtins/src/lang_class.rs::synthetic_jdk_method_decls}
 *       surfaces a non-empty {@code Method[]} for synthetic JDBC
 *       interfaces (Connection, ResultSet) — the load-bearing surface
 *       that frameworks holding real JDBC bytecode (HikariCP wrapping,
 *       Spring/Hibernate proxies) introspect.</li>
 *   <li>{@code Method.toString()} and {@code Method.getName()} round-trip
 *       to non-null Strings (their natives must already be registered
 *       and dispatchable).</li>
 * </ol>
 *
 * <h2>Why a separate fixture from {@code Wp72JdbcCoreTypes}</h2>
 * The WP7.2 fixture mixes JDBC reachability checks with the reflection
 * deep-probe. This fixture isolates the WP2.1 acceptance to a minimum
 * surface — Connection + ResultSet — and exposes counts so the Rust
 * harness can assert {@code length > 0} cleanly, not just
 * {@code returned-1-on-pass}.
 *
 * Following the existing {@code WpN_M*} convention, every method
 * returns a small int that the Rust harness asserts on.
 */
public class Wp21ClassReflectE2E {

    /**
     * Returns the number of declared methods on {@code Connection}, or
     * a negative sentinel on failure:
     *   - -1 if {@code getDeclaredMethods()} threw.
     *   - -2 if the returned array was null.
     *   - -3 if any method had a null {@code getName()}.
     *   - -4 if any method had a null {@code toString()}.
     */
    public static int connectionDeclaredMethodCount() {
        try {
            Class<?> c = Connection.class;
            Method[] ms = c.getDeclaredMethods();
            if (ms == null) return -2;
            for (Method m : ms) {
                if (m.getName() == null) return -3;
                if (m.toString() == null) return -4;
            }
            return ms.length;
        } catch (Throwable t) {
            return -1;
        }
    }

    /**
     * Returns 1 if {@code ResultSet.class.getDeclaredMethods()} surfaces
     * a {@code next()} method whose return type is the {@code boolean}
     * primitive class. Returns 0 on any other outcome (including throw).
     *
     * Pins the {@code Method.getReturnType()} round-trip on a synthetic
     * stub interface — frameworks that introspect SPI methods by return
     * shape (Spring's {@code RowMapper}, Hibernate's connection proxy)
     * rely on this.
     */
    public static int resultSetNextReturnsBoolean() {
        try {
            Method[] ms = ResultSet.class.getDeclaredMethods();
            if (ms == null || ms.length == 0) return 0;
            for (Method m : ms) {
                if ("next".equals(m.getName()) && m.getParameterCount() == 0) {
                    return m.getReturnType() == boolean.class ? 1 : 0;
                }
            }
            return 0;
        } catch (Throwable t) {
            return 0;
        }
    }

    /**
     * Composite probe: returns 1 iff both Connection and ResultSet
     * surface non-empty declared method arrays AND every Method on each
     * has non-null {@code getName()} + {@code toString()}.
     *
     * Mirrors the WP7.2 best-effort probe but as a single hard pass/fail
     * for the WP2.1-narrow acceptance. Frameworks driving HikariCP /
     * ByteBuddy / Spring JDBC stub wrapping see exactly this surface
     * shape from real bytecode.
     */
    public static int closesGetDeclaredMethodsGap() {
        Class<?>[] probe = new Class<?>[] {
            Connection.class,
            ResultSet.class,
        };
        try {
            for (Class<?> c : probe) {
                Method[] ms = c.getDeclaredMethods();
                if (ms == null || ms.length == 0) return 0;
                for (Method m : ms) {
                    String n = m.getName();
                    if (n == null || n.length() == 0) return 0;
                    String s = m.toString();
                    if (s == null) return 0;
                }
            }
            return 1;
        } catch (Throwable t) {
            return 0;
        }
    }
}
