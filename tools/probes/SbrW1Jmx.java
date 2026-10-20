import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.management.BufferPoolMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Hashtable;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.TreeMap;
import java.util.TreeSet;
import javax.management.MBeanServer;
import javax.management.ObjectName;

/**
 * SBR wave 1, lane JMX: the retired {@code javax/management/ObjectName}
 * surface (every row of {@code RETIRED_SHADOW_SBR_JMX_TRIPLES}) plus the
 * native producers and readers of an ObjectName that survive the retirement
 * (platform MXBean {@code getObjectName}, BufferPool {@code getObjectName},
 * the platform MBeanServer's {@code queryNames}/{@code isRegistered}).
 *
 * Prints every value from the FIRST run, then recomputes the whole signature
 * in a hot loop and counts lines that differ from the first run
 * ({@code drift}). Diff the output against HotSpot 25.
 */
public class SbrW1Jmx {
    static final String[] NAMES = {
        "d:b=2,a=1,c=3",
        "cratonvm.test:type=Counter,name=alpha",
        "java.lang:type=GarbageCollector,name=G1 Young Generation",
        "d:k=v,*",
        "d:*",
        "*:*",
        "java.*:type=Memory",
        "d?:type=x",
        "d:k=*",
        "d:k=a?c,j=v",
        "d:k=\"a,b=c\"",
        "d:k=\"x*y\",*",
        ":type=NoDomain",
        "d:z=1,y=2,*,x=3",
    };

    static final String[] BAD = {
        "not a valid name", "d:", "d:k", "d:k=v,k=w", "d:=v", "d:k=v,,j=w", "d:k=a:b",
    };

    static String row(ObjectName n) {
        StringBuilder sb = new StringBuilder();
        sb.append("toString=").append(n.toString());
        sb.append(" canon=").append(n.getCanonicalName());
        sb.append(" domain=").append(n.getDomain());
        sb.append(" kpls=").append(n.getKeyPropertyListString());
        sb.append(" ckpls=").append(n.getCanonicalKeyPropertyListString());
        sb.append(" kpl=").append(new TreeMap<>(n.getKeyPropertyList()));
        sb.append(" pat=").append(n.isPattern()).append(n.isDomainPattern())
                .append(n.isPropertyPattern()).append(n.isPropertyListPattern())
                .append(n.isPropertyValuePattern());
        sb.append(" hash=").append(n.hashCode());
        String k = n.getKeyPropertyList().isEmpty() ? "none"
                : new TreeSet<>(n.getKeyPropertyList().keySet()).first();
        sb.append(" kp[").append(k).append("]=").append(n.getKeyProperty(k));
        sb.append(" same=").append(ObjectName.getInstance(n) == n);
        return sb.toString();
    }

    static List<String> signature() throws Exception {
        List<String> out = new ArrayList<>();
        List<ObjectName> made = new ArrayList<>();
        for (String s : NAMES) {
            ObjectName a = new ObjectName(s);
            ObjectName b = ObjectName.getInstance(s);
            made.add(a);
            out.add("name " + s + " | " + row(a) + " | eqInst=" + a.equals(b)
                    + " hashInst=" + (a.hashCode() == b.hashCode()));
        }
        // apply(): every pattern against every name.
        StringBuilder ap = new StringBuilder();
        for (ObjectName p : made) {
            ap.append(p.isPattern() ? "P" : "-");
            for (ObjectName c : made) {
                ap.append(p.apply(c) ? '1' : '0');
            }
            ap.append(' ');
        }
        out.add("apply " + ap);
        // equality across key order.
        ObjectName x = new ObjectName("q:a=1,b=2");
        ObjectName y = new ObjectName("q:b=2,a=1");
        out.add("eq keyorder=" + x.equals(y) + " hash=" + (x.hashCode() == y.hashCode())
                + " cmp=" + Integer.signum(x.compareTo(y)));
        // the other two constructors.
        out.add("ctor3 " + row(new ObjectName("dom", "key", "val")));
        Hashtable<String, String> t = new Hashtable<>();
        t.put("type", "T");
        t.put("name", "n1");
        t.put("alpha", "z");
        out.add("ctorHT " + row(new ObjectName("dom", t)));
        out.add("getInst3 " + row(ObjectName.getInstance("dom", "key", "val")));
        // quote / unquote.
        for (String q : new String[] {"a,b=c", "x*y?z", "back\\slash", "q\"t", "nl\nx"}) {
            String quoted = ObjectName.quote(q);
            ObjectName n = new ObjectName("d:k=" + quoted);
            out.add("quote " + quoted.replace("\n", "\\n") + " -> kp="
                    + n.getKeyProperty("k").replace("\n", "\\n") + " unq="
                    + ObjectName.unquote(n.getKeyProperty("k")).equals(q)
                    + " canon=" + n.getCanonicalName().replace("\n", "\\n"));
        }
        // malformed names: class and message.
        for (String s : BAD) {
            try {
                new ObjectName(s);
                out.add("bad " + s + " -> accepted");
            } catch (Exception e) {
                out.add("bad " + s + " -> " + e.getClass().getName() + ": " + e.getMessage());
            }
        }
        // serialization round trip.
        for (String s : new String[] {"d:b=2,a=1,c=3", "d:k=v,*", "java.*:type=Memory"}) {
            ObjectName n = new ObjectName(s);
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
                oos.writeObject(n);
            }
            ObjectName r;
            try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
                r = (ObjectName) ois.readObject();
            }
            out.add("ser " + s + " | " + row(r) + " | eq=" + r.equals(n));
        }
        // native PRODUCERS that survive: platform bean + buffer pool getObjectName.
        ObjectName rt = ManagementFactory.getRuntimeMXBean().getObjectName();
        out.add("rt " + row(rt));
        out.add("th " + row(ManagementFactory.getThreadMXBean().getObjectName()));
        out.add("os " + row(ManagementFactory.getOperatingSystemMXBean().getObjectName()));
        out.add("cl " + row(ManagementFactory.getClassLoadingMXBean().getObjectName()));
        out.add("mem " + row(ManagementFactory.getMemoryMXBean().getObjectName()));
        List<String> pools = new ArrayList<>();
        for (BufferPoolMXBean p : ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)) {
            pools.add(p.getName() + "=" + row(p.getObjectName()));
        }
        Collections.sort(pools);
        out.add("pools " + pools);
        // native READERS: the platform MBeanServer.
        MBeanServer mbs = ManagementFactory.getPlatformMBeanServer();
        out.add("mbs reg rt=" + mbs.isRegistered(rt)
                + " reg runtime=" + mbs.isRegistered(new ObjectName("java.lang:type=Runtime"))
                + " reg bp=" + mbs.isRegistered(new ObjectName("java.nio:name=direct,type=BufferPool")));
        Set<ObjectName> q = mbs.queryNames(new ObjectName("java.lang:type=Runtime,*"), null);
        List<String> qs = new ArrayList<>();
        for (ObjectName n : q) {
            qs.add(n.toString() + "/" + n.getKeyPropertyListString());
        }
        Collections.sort(qs);
        out.add("mbs query runtime " + qs);
        Set<ObjectName> bpq = mbs.queryNames(new ObjectName("java.nio:type=BufferPool,*"), null);
        List<String> bps = new ArrayList<>();
        for (ObjectName n : bpq) {
            bps.add(n.getCanonicalName());
        }
        Collections.sort(bps);
        out.add("mbs query bufferpool " + bps);
        Map<String, Object> attr = new TreeMap<>();
        attr.put("rtName", mbs.getAttribute(new ObjectName("java.lang:type=Runtime"), "Name")
                .equals(ManagementFactory.getRuntimeMXBean().getName()));
        out.add("mbs attr " + attr);
        return out;
    }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        List<String> first = signature();
        for (String s : first) {
            System.out.println(s);
        }
        int drift = 0;
        String firstDrift = null;
        for (int i = 0; i < iters; i++) {
            List<String> again = signature();
            for (int j = 0; j < first.size(); j++) {
                if (j >= again.size() || !first.get(j).equals(again.get(j))) {
                    drift++;
                    if (firstDrift == null) {
                        firstDrift = "iter " + i + " line " + j + ": "
                                + (j < again.size() ? again.get(j) : "<missing>");
                    }
                }
            }
        }
        System.out.println("drift " + drift + " over " + iters + " iterations");
        if (firstDrift != null) {
            System.out.println("firstDrift " + firstDrift);
        }
    }
}
