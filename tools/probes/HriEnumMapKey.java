import java.util.*;
import java.util.stream.*;

/**
 * An enum-keyed HashMap filled by JDK bytecode must keep answering get() from
 * compiled code across collections. Under --compatible the compiled
 * HashMap.get helper fell back, after a GC emptied its memo caches, to the
 * native lookup, which hashed an enum key by (class name, constant name)
 * instead of Enum.hashCode() (the identity hash): whole invocations of hot()
 * missed. Hibernate Reactive's TenantDependentPool.MultiplePools does
 * poolMap.get(Tenant.DEFAULT) and NPE'd. Every row must match HotSpot 25.
 */
public class HriEnumMapKey {
    enum Tenant { DEFAULT, TENANT_1, TENANT_2 }

    static Object sink;

    static long hot(Map<Tenant, String> m, Map<Tenant, String> hm) {
        long miss = 0;
        for (int i = 0; i < 20000; i++) {
            Tenant t = Tenant.values()[i % 3];
            if (m.get(t) == null) miss += 1;
            if (hm.get(Tenant.DEFAULT) == null) miss += 1L << 20;
            if (t.hashCode() != System.identityHashCode(t)) miss += 1L << 40;
            sink = new Object[4];
        }
        return miss;
    }

    public static void main(String[] args) {
        Map<Tenant, String> m = Stream.of(Tenant.values())
                .collect(Collectors.toMap(t -> t, Enum::name));
        Map<Tenant, String> hm = new HashMap<>(m);
        int h0 = Tenant.DEFAULT.hashCode();
        int i0 = System.identityHashCode(Tenant.DEFAULT);
        for (int r = 0; r < 40; r++) {
            List<byte[]> l = new ArrayList<>();
            for (int i = 0; i < 20000; i++) l.add(new byte[256]);
            sink = l;
            if (r % 10 == 0) System.gc();
        }
        System.out.println("class=" + m.getClass().getName());
        System.out.println("hash.stable=" + (h0 == Tenant.DEFAULT.hashCode()));
        System.out.println("identity.stable=" + (i0 == System.identityHashCode(Tenant.DEFAULT)));
        System.out.println("hash==identity=" + (Tenant.DEFAULT.hashCode() == System.identityHashCode(Tenant.DEFAULT)));
        for (Tenant t : Tenant.values()) {
            System.out.println("toMap.get(" + t + ")=" + m.get(t) + " hashMap.get=" + hm.get(t)
                    + " containsKey=" + m.containsKey(t));
        }
        System.out.println("size=" + m.size());
        long miss = 0;
        for (int r = 0; r < 200; r++) {
            miss += hot(m, hm);
            if (r % 50 == 0) System.gc();
        }
        System.out.println("hot.misses.toMap=" + (miss & 0xFFFFF) + " hashMap=" + ((miss >> 20) & 0xFFFFF) + " hash!=identity=" + (miss >> 40));
    }
}
