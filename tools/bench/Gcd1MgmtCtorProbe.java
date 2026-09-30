// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryManagerMXBean;
import java.lang.management.MemoryUsage;
import java.lang.reflect.Constructor;
import java.util.ArrayList;
import java.util.List;
import javax.management.NotificationEmitter;
import javax.management.NotificationListener;

/**
 * gcd d1/e (2026-09-27): the management constructors run the JDK's own
 * bytecode in {@code --compatible} mode. Page:
 * {@code docs/internal/gc/gengc-r5w4-obs8-management-constructor-bridges-shadow-the-jdk-constructors-FIXED-20260928.md}.
 *
 * <p>Checks, against HotSpot Serial:
 * <ol>
 *   <li>{@code new MemoryUsage(init, used, committed, max)} rejects each
 *       invalid argument combination with {@code IllegalArgumentException}
 *       and keeps a valid one;
 *   <li>every collector bean takes and drops a notification listener (the
 *       {@code NotificationEmitterSupport} fields exist);
 *   <li>a {@code sun.management.GarbageCollectorImpl} and a
 *       {@code MemoryManagerImpl} built by reflection (the constructor chain
 *       itself, not the VM's factory) do the same, and report their name and
 *       {@code isValid() == true}.
 * </ol>
 * Deterministic stdout on HotSpot Serial:
 * <pre>
 *   memoryusage-used-over-committed IllegalArgumentException
 *   memoryusage-init-below-minus-one IllegalArgumentException
 *   memoryusage-negative-used IllegalArgumentException
 *   memoryusage-negative-committed IllegalArgumentException
 *   memoryusage-max-below-minus-one IllegalArgumentException
 *   memoryusage-committed-over-max IllegalArgumentException
 *   memoryusage-valid 1 2 3 4
 *   memoryusage-undefined-max -1 0 0 -1
 *   collector-listener Copy ok
 *   collector-listener MarkSweepCompact ok
 *   reflect-collector probe-gc valid=true listener=ok
 *   reflect-manager probe-mgr valid=true listener=ok
 *   PASS
 * </pre>
 * Before gcd d1/e CratonVM printed {@code accepted} on the six
 * {@code memoryusage-*} rejection lines, and the two {@code reflect-*}
 * listener checks printed {@code listener=NullPointerException}. Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m --add-opens java.management/sun.management=ALL-UNNAMED -cp tools/bench Gcd1MgmtCtorProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m --add-opens java.management/sun.management=ALL-UNNAMED -cp tools/bench Gcd1MgmtCtorProbe
 * </pre>
 */
public final class Gcd1MgmtCtorProbe {
    static int failures;

    static void reject(String label, long init, long used, long committed, long max) {
        try {
            MemoryUsage u = new MemoryUsage(init, used, committed, max);
            System.out.println(label + " accepted " + u.getUsed());
            failures++;
        } catch (IllegalArgumentException e) {
            System.out.println(label + " IllegalArgumentException");
        }
    }

    static void keep(String label, long init, long used, long committed, long max) {
        try {
            MemoryUsage u = new MemoryUsage(init, used, committed, max);
            System.out.println(label + " " + u.getInit() + " " + u.getUsed() + " "
                + u.getCommitted() + " " + u.getMax());
            if (u.getInit() != init || u.getUsed() != used
                    || u.getCommitted() != committed || u.getMax() != max) {
                failures++;
            }
        } catch (IllegalArgumentException e) {
            System.out.println(label + " IllegalArgumentException");
            failures++;
        }
    }

    /** "ok" when {@code bean} takes and drops a listener, else the exception's simple name. */
    static String listener(Object bean) {
        NotificationListener l = (n, hb) -> { };
        try {
            NotificationEmitter e = (NotificationEmitter) bean;
            e.addNotificationListener(l, null, null);
            e.removeNotificationListener(l);
            return "ok";
        } catch (Exception | Error e) {
            failures++;
            return e.getClass().getSimpleName();
        }
    }

    static void reflect(String label, String className, String name) {
        try {
            Class<?> c = Class.forName(className);
            Constructor<?> k = c.getDeclaredConstructor(String.class);
            k.setAccessible(true);
            MemoryManagerMXBean bean = (MemoryManagerMXBean) k.newInstance(name);
            boolean valid = bean.isValid();
            String got = bean.getName();
            if (!valid || !name.equals(got)) {
                failures++;
            }
            System.out.println(label + " " + got + " valid=" + valid
                + " listener=" + listener(bean));
        } catch (ReflectiveOperationException | RuntimeException e) {
            failures++;
            System.out.println(label + " " + e.getClass().getSimpleName());
        }
    }

    public static void main(String[] args) {
        reject("memoryusage-used-over-committed", 0, 10, 5, 100);
        reject("memoryusage-init-below-minus-one", -2, 0, 0, 0);
        reject("memoryusage-negative-used", 0, -1, 0, 0);
        reject("memoryusage-negative-committed", 0, 0, -1, 0);
        reject("memoryusage-max-below-minus-one", 0, 0, 0, -2);
        reject("memoryusage-committed-over-max", 0, 5, 10, 8);
        keep("memoryusage-valid", 1, 2, 3, 4);
        keep("memoryusage-undefined-max", -1, 0, 0, -1);

        List<String> names = new ArrayList<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            names.add(gc.getName() + " " + listener(gc));
        }
        names.sort(null);
        for (String s : names) {
            System.out.println("collector-listener " + s);
        }

        reflect("reflect-collector", "sun.management.GarbageCollectorImpl", "probe-gc");
        reflect("reflect-manager", "sun.management.MemoryManagerImpl", "probe-mgr");
        System.out.println(failures == 0 ? "PASS" : "FAIL " + failures);
    }
}
