import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.management.OperatingSystemMXBean;
import java.lang.management.RuntimeMXBean;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.Set;
import javax.management.Attribute;
import javax.management.AttributeNotFoundException;
import javax.management.InstanceNotFoundException;
import javax.management.MBeanAttributeInfo;
import javax.management.MBeanInfo;
import javax.management.MBeanOperationInfo;
import javax.management.MBeanServer;
import javax.management.MalformedObjectNameException;
import javax.management.Notification;
import javax.management.NotificationBroadcasterSupport;
import javax.management.NotificationListener;
import javax.management.ObjectName;

/**
 * JDK-only corpus: JMX -- {@code ManagementFactory}, {@code ObjectName}, MBean
 * registration and invocation.
 *
 * This is a named P0 blocker ("JMX real path" in
 * docs/jdk-only-runtime-services.md): with stubs dropped, the first
 * {@code getPlatformMBeanServer()} NPEs inside real {@code javax.management}
 * bytecode with {@code ObjectName._ca_array} null. This vector is the smallest
 * thing that reproduces that shape, so it is the closure test for that row.
 *
 * Determinism: no counts, sizes, uptimes, pids or host names are printed --
 * every one of those varies per run. Only our own MBean's metadata and
 * behaviour, plus presence/shape predicates on the platform beans.
 */
public class RJdkJmx {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    // A standard MBean: the interface name must be <impl> + "MBean".
    public interface CounterMBean {
        int getValue();

        void setValue(int v);

        String getName();

        int addAndGet(int delta);

        String describe(String prefix, int n);
    }

    public static class Counter extends NotificationBroadcasterSupport implements CounterMBean {
        private int value;
        private final String name;
        private long seq;

        public Counter(String name, int value) {
            this.name = name;
            this.value = value;
        }

        @Override
        public int getValue() {
            return value;
        }

        @Override
        public void setValue(int v) {
            int old = value;
            value = v;
            sendNotification(new Notification("counter.set", this, ++seq, "old=" + old));
        }

        @Override
        public String getName() {
            return name;
        }

        @Override
        public int addAndGet(int delta) {
            value += delta;
            return value;
        }

        @Override
        public String describe(String prefix, int n) {
            return prefix + ":" + name + ":" + (value * n);
        }
    }

    static void objectNames() throws Exception {
        // ObjectName parsing and canonicalisation: key order is normalised.
        ObjectName a = new ObjectName("cratonvm.test:type=Counter,name=alpha");
        ObjectName b = new ObjectName("cratonvm.test:name=alpha,type=Counter");
        check(a.equals(b), "ObjectName equality is key-order independent");
        check(a.getCanonicalName().equals("cratonvm.test:name=alpha,type=Counter"),
                "canonical name sorts keys: " + a.getCanonicalName());
        check(a.getDomain().equals("cratonvm.test"), "domain");
        check(a.getKeyProperty("type").equals("Counter"), "getKeyProperty");
        check(a.getKeyPropertyList().size() == 2, "key property list size");
        check(!a.isPattern() && !a.isDomainPattern() && !a.isPropertyPattern(), "not a pattern");
        check(a.hashCode() == b.hashCode(), "ObjectName hashCode");

        ObjectName pattern = new ObjectName("cratonvm.test:type=Counter,*");
        check(pattern.isPattern() && pattern.isPropertyPattern(), "property pattern");
        check(pattern.apply(a), "pattern must match");
        check(!pattern.apply(new ObjectName("other:type=Counter")), "pattern must not over-match");

        ObjectName quoted = new ObjectName("d:k=" + ObjectName.quote("a,b=c"));
        check(ObjectName.unquote(quoted.getKeyProperty("k")).equals("a,b=c"), "quote/unquote");

        boolean threw = false;
        try {
            new ObjectName("not a valid name");
        } catch (MalformedObjectNameException expected) {
            threw = true;
        }
        check(threw, "a malformed ObjectName must throw MalformedObjectNameException");
        System.out.println("CK RJdkJmx objectName=" + a.getCanonicalName());
    }

    static void registerAndInvoke() throws Exception {
        MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        check(mbs != null, "getPlatformMBeanServer returned null");
        check(mbs.getDefaultDomain().equals("DefaultDomain"), "default domain: "
                + mbs.getDefaultDomain());
        // The platform server is a singleton.
        check(mbs == ManagementFactory.getPlatformMBeanServer(), "platform MBeanServer singleton");

        ObjectName on = new ObjectName("cratonvm.test:type=Counter,name=alpha");
        if (mbs.isRegistered(on)) {
            mbs.unregisterMBean(on);
        }
        Counter counter = new Counter("alpha", 10);
        mbs.registerMBean(counter, on);
        check(mbs.isRegistered(on), "registerMBean did not register");

        // Attributes.
        check(((Integer) mbs.getAttribute(on, "Value")) == 10, "getAttribute");
        check("alpha".equals(mbs.getAttribute(on, "Name")), "getAttribute (read-only)");
        mbs.setAttribute(on, new Attribute("Value", 25));
        check(((Integer) mbs.getAttribute(on, "Value")) == 25, "setAttribute");
        check(counter.getValue() == 25, "setAttribute reached the real object");

        boolean threw = false;
        try {
            mbs.getAttribute(on, "NoSuchAttribute");
        } catch (AttributeNotFoundException expected) {
            threw = true;
        }
        check(threw, "an unknown attribute must raise AttributeNotFoundException");

        // Operations.
        check(((Integer) mbs.invoke(on, "addAndGet", new Object[] { 5 },
                new String[] { "int" })) == 30, "invoke with a primitive argument");
        check("p:alpha:60".equals(mbs.invoke(on, "describe",
                new Object[] { "p", 2 }, new String[] { "java.lang.String", "int" })),
                "invoke with mixed arguments");

        // Metadata: attribute/operation names are sorted before assertion, the
        // MBeanServer does not promise an order.
        MBeanInfo info = mbs.getMBeanInfo(on);
        check(info.getClassName().equals(Counter.class.getName()), "MBeanInfo class name: "
                + info.getClassName());
        List<String> attrs = new ArrayList<>();
        for (MBeanAttributeInfo ai : info.getAttributes()) {
            attrs.add(ai.getName() + (ai.isReadable() ? "r" : "") + (ai.isWritable() ? "w" : ""));
        }
        Collections.sort(attrs);
        check(attrs.equals(Arrays.asList("Namer", "Valuerw")), "attributes: " + attrs);
        List<String> ops = new ArrayList<>();
        for (MBeanOperationInfo oi : info.getOperations()) {
            ops.add(oi.getName() + "/" + oi.getSignature().length);
        }
        Collections.sort(ops);
        check(ops.equals(Arrays.asList("addAndGet/1", "describe/2")), "operations: " + ops);

        // Notifications.
        final List<String> got = new ArrayList<>();
        NotificationListener listener = (n, handback) ->
                got.add(n.getType() + "|" + n.getMessage() + "|" + handback);
        mbs.addNotificationListener(on, listener, null, "HB");
        mbs.setAttribute(on, new Attribute("Value", 7));
        mbs.removeNotificationListener(on, listener);
        mbs.setAttribute(on, new Attribute("Value", 8));
        check(got.equals(Collections.singletonList("counter.set|old=30|HB")),
                "notification delivery: " + got);

        // Queries.
        Set<ObjectName> found = mbs.queryNames(new ObjectName("cratonvm.test:*"), null);
        check(found.size() == 1 && found.contains(on), "queryNames: " + found);
        check(mbs.queryNames(new ObjectName("no.such.domain:*"), null).isEmpty(),
                "queryNames on an empty domain");
        check(Arrays.asList(mbs.getDomains()).contains("cratonvm.test"), "getDomains");

        // Unregistration.
        mbs.unregisterMBean(on);
        check(!mbs.isRegistered(on), "unregisterMBean");
        threw = false;
        try {
            mbs.getAttribute(on, "Value");
        } catch (InstanceNotFoundException expected) {
            threw = true;
        }
        check(threw, "access after unregister must raise InstanceNotFoundException");
        System.out.println("CK RJdkJmx attrs=" + attrs + " ops=" + ops + " notif=" + got);
    }

    /** Platform MXBeans must be REAL beans, not fabricated shells. */
    static void platformBeans() throws Exception {
        RuntimeMXBean rt = ManagementFactory.getRuntimeMXBean();
        check(rt != null, "RuntimeMXBean");
        check(rt.getName() != null && !rt.getName().isEmpty(), "RuntimeMXBean.getName");
        check(rt.getStartTime() > 0, "RuntimeMXBean.getStartTime");
        check(rt.getInputArguments() != null, "RuntimeMXBean.getInputArguments");
        check(rt.getObjectName().getCanonicalName().equals("java.lang:type=Runtime"),
                "RuntimeMXBean ObjectName: " + rt.getObjectName());

        MemoryMXBean mem = ManagementFactory.getMemoryMXBean();
        check(mem.getHeapMemoryUsage().getMax() != 0, "heap max");
        check(mem.getHeapMemoryUsage().getUsed() > 0, "heap used");

        ThreadMXBean th = ManagementFactory.getThreadMXBean();
        check(th.getThreadCount() > 0, "thread count");
        check(th.getAllThreadIds().length > 0, "thread id list");

        ClassLoadingMXBean cl = ManagementFactory.getClassLoadingMXBean();
        check(cl.getLoadedClassCount() > 0, "loaded class count");

        OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
        check(os.getAvailableProcessors() > 0, "available processors");
        check(os.getName() != null, "os name");

        // The platform server must expose them under their canonical names.
        MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        check(mbs.isRegistered(new ObjectName("java.lang:type=Runtime")), "java.lang:type=Runtime");
        check(mbs.isRegistered(new ObjectName("java.lang:type=Memory")), "java.lang:type=Memory");
        check(mbs.isRegistered(new ObjectName("java.lang:type=Threading")),
                "java.lang:type=Threading");
        check(((Integer) mbs.getAttribute(new ObjectName("java.lang:type=OperatingSystem"),
                "AvailableProcessors")) == os.getAvailableProcessors(),
                "proxy read of a platform attribute must agree with the direct read");

        // A platform MXBean proxy is a dynamic proxy over the server.
        RuntimeMXBean proxy = ManagementFactory.getPlatformMXBean(mbs, RuntimeMXBean.class);
        check(proxy.getStartTime() == rt.getStartTime(), "platform MXBean proxy agrees");

        List<String> beans = new ArrayList<>();
        for (Class<?> c : new Class<?>[] { RuntimeMXBean.class, MemoryMXBean.class,
                ThreadMXBean.class, ClassLoadingMXBean.class, OperatingSystemMXBean.class }) {
            beans.add(c.getSimpleName());
        }
        Collections.sort(beans);
        System.out.println("CK RJdkJmx platform=" + beans
                + " runtimeName=" + rt.getObjectName().getCanonicalName());
    }

    public static void main(String[] args) throws Exception {
        objectNames();
        registerAndInvoke();
        platformBeans();
        System.out.println("CK RJdkJmx checks=" + checks);
        System.out.println("PASS RJdkJmx (" + checks + " checks)");
    }
}
