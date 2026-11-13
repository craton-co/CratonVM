// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i7 wave 138, lane L5 (page
// docs/feature-designs/zgc-architecture.md):
// the delegation doors for `sun.reflect.misc.Trampoline`, a `java.base` class
// that `sun.reflect.misc.MethodUtil` also defines in its own loader. HotSpot's
// built-in loaders, and any loader whose parent chain ends at the bootstrap
// loader, answer the BOOT copy (loader null); JMX keeps working through
// MethodUtil's copy. The `is_bootstrap_class_name` / `is_runtime_image_class_name`
// name exceptions are gone; the boot doors answer exactly
// (CRATONVM_BOOT_LOOKUP_EXACT, default ON, `=0` restores the old doors).
//
// Run with no argument (JMX first: MethodUtil's copy exists before any boot
// lookup) and with `boot-first` (the boot copy exists first):
//      javac -d out L5W138TrampolineBootDoors.java
//      java -cp out L5W138TrampolineBootDoors [boot-first]                 (HotSpot)
//      cratonvm --real-jdk|--compatible [--nojit] -cp out L5W138TrampolineBootDoors [boot-first]
// Both orders print the same lines on HotSpot (the `order` line aside).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint, both orders)
// order: jmx-first
// jmx-1: value=7 op=14
// system: sun.reflect.misc.Trampoline loader=null
// platform: sun.reflect.misc.Trampoline loader=null
// null-parent: sun.reflect.misc.Trampoline loader=null
// forname-system: sun.reflect.misc.Trampoline loader=null
// forname-null: sun.reflect.misc.Trampoline loader=null
// same-class: true
// jmx-2: value=7 op=14
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import javax.management.MBeanServer;
import javax.management.ObjectName;

public class L5W138TrampolineBootDoors {
    public interface ProbeMBean {
        int getValue();

        int twice(int x);
    }

    public static class Probe implements ProbeMBean {
        @Override
        public int getValue() {
            return 7;
        }

        @Override
        public int twice(int x) {
            return 2 * x;
        }
    }

    static final String T = "sun.reflect.misc.Trampoline";

    static String jmx(MBeanServer server, ObjectName name) {
        try {
            Object value = server.getAttribute(name, "Value");
            Object op = server.invoke(name, "twice", new Object[] {7}, new String[] {"int"});
            return "value=" + value + " op=" + op;
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static Class<?> last;

    static String load(ClassLoader l) {
        try {
            Class<?> c = l.loadClass(T);
            last = c;
            return c.getName() + " loader=" + c.getClassLoader();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String forName(ClassLoader l) {
        try {
            Class<?> c = Class.forName(T, false, l);
            return c.getName() + " loader=" + c.getClassLoader();
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    public static void main(String[] args) throws Exception {
        boolean bootFirst = args.length > 0 && args[0].equals("boot-first");
        MBeanServer server = ManagementFactory.getPlatformMBeanServer();
        ObjectName name = new ObjectName("l5w138:type=Probe");
        server.registerMBean(new Probe(), name);
        String sys = null;
        if (bootFirst) {
            sys = load(ClassLoader.getSystemClassLoader());
        }
        String jmx1 = jmx(server, name);
        if (!bootFirst) {
            sys = load(ClassLoader.getSystemClassLoader());
        }
        Class<?> fromSystem = last;
        System.out.println("order: jmx-first");
        System.out.println("jmx-1: " + jmx1);
        System.out.println("system: " + sys);
        System.out.println("platform: " + load(ClassLoader.getPlatformClassLoader()));
        System.out.println("null-parent: " + load(new ClassLoader(null) { }));
        System.out.println("forname-system: " + forName(ClassLoader.getSystemClassLoader()));
        System.out.println("forname-null: " + forName(null));
        System.out.println("same-class: " + (fromSystem != null && fromSystem == last));
        System.out.println("jmx-2: " + jmx(server, name));
    }
}
