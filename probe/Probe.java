import java.io.FileWriter;
import java.io.IOException;
import java.lang.reflect.*;
import java.util.*;

public class Probe {
    public static class A {
        private int y;
        public int getY() { return y; }
        public void setY(int v) { y = v; }
    }

    static StringBuilder out = new StringBuilder();
    static void p(String s) { out.append(s).append('\n'); }

    public static void main(String[] args) throws Exception {
        Class<?> a = A.class;
        p("class name = " + a.getName());
        p("getSuperclass = " + a.getSuperclass());
        p("getGenericSuperclass = " + a.getGenericSuperclass());
        p("getInterfaces.len = " + a.getInterfaces().length);
        p("getGenericInterfaces.len = " + a.getGenericInterfaces().length);

        // getDeclaredMethods: count getY + identity
        Method[] dm = a.getDeclaredMethods();
        p("getDeclaredMethods.len = " + dm.length);
        List<Method> getYs = new ArrayList<>();
        for (Method m : dm) {
            if (m.getName().equals("getY")) getYs.add(m);
        }
        p("declared getY count = " + getYs.size());
        for (Method m : getYs) {
            p("  getY identityHash=" + System.identityHashCode(m)
              + " name.hashCode=" + m.getName().hashCode()
              + " paramCount=" + m.getParameterCount()
              + " paramTypes.len=" + m.getParameterTypes().length);
        }

        // call getName twice on same method, compare
        if (!getYs.isEmpty()) {
            Method m = getYs.get(0);
            String n1 = m.getName(); String n2 = m.getName();
            p("getName ==: " + (n1 == n2) + " equals:" + n1.equals(n2)
              + " hc1=" + n1.hashCode() + " hc2=" + n2.hashCode());
            Class<?>[] pt1 = m.getParameterTypes();
            Class<?>[] pt2 = m.getParameterTypes();
            p("getParameterTypes len1=" + pt1.length + " len2=" + pt2.length);
        }

        // getMethods (public) count
        Method[] pm = a.getMethods();
        int gm = 0;
        Method first = null;
        for (Method m : pm) if (m.getName().equals("getY")) { gm++; if(first==null) first=m; }
        p("getMethods getY count = " + gm);

        // Emulate Jackson hierarchy walk: collect all super types like ClassUtil.findSuperTypes
        // (superclass chain + all interfaces, recursively)
        List<Class<?>> hierarchy = new ArrayList<>();
        collectSuperTypes(a, hierarchy);
        p("hierarchy (findSuperTypes-style), size=" + hierarchy.size() + ":");
        Map<Class<?>,Integer> seen = new HashMap<>();
        for (Class<?> c : hierarchy) {
            seen.merge(c, 1, Integer::sum);
            p("   " + c.getName() + " idHash=" + System.identityHashCode(c));
        }
        for (Map.Entry<Class<?>,Integer> e : seen.entrySet())
            if (e.getValue() > 1) p("   DUPLICATE in hierarchy: " + e.getKey().getName() + " x" + e.getValue());

        // Emulate AnnotatedMethodCollector dedup: walk main + supertypes, key by (name,paramTypes)
        // exactly like Jackson MemberKey
        List<Class<?>> all = new ArrayList<>();
        all.add(a);
        all.addAll(hierarchy);
        Map<MemberKey, Method> methods = new LinkedHashMap<>();
        int collisions = 0, inserts = 0;
        for (Class<?> c : all) {
            for (Method m : c.getDeclaredMethods()) {
                MemberKey k = new MemberKey(m);
                if (methods.containsKey(k)) { collisions++; }
                else { methods.put(k, m); inserts++; }
            }
        }
        int memY = 0;
        for (Map.Entry<MemberKey,Method> e : methods.entrySet())
            if (e.getKey().name.equals("getY")) memY++;
        p("EMULATED collector: inserts=" + inserts + " collisions=" + collisions
          + " memberMethods getY count = " + memY);

        // Check: build two MemberKeys for the SAME method, are they equal? same hashCode? collide in map?
        if (!getYs.isEmpty()) {
            Method m = getYs.get(0);
            MemberKey k1 = new MemberKey(m), k2 = new MemberKey(m);
            p("two MemberKeys same method: equals=" + k1.equals(k2)
              + " hc1=" + k1.hashCode() + " hc2=" + k2.hashCode());
            HashMap<MemberKey,Integer> hm = new HashMap<>();
            hm.put(k1, 1);
            p("   map.containsKey(k2)=" + hm.containsKey(k2) + " map.get(k2)=" + hm.get(k2));
        }

        try (FileWriter fw = new FileWriter("probe_out.txt")) {
            fw.write(out.toString());
        }
        System.out.println(out.toString());
    }

    static void collectSuperTypes(Class<?> c, List<Class<?>> result) {
        // mimic ClassUtil._addSuperTypes: interfaces then superclass, recursive, includes Object
        Class<?> sup = c.getSuperclass();
        for (Class<?> itf : c.getInterfaces()) {
            if (!result.contains(itf)) { result.add(itf); collectSuperTypes(itf, result); }
        }
        if (sup != null) {
            if (!result.contains(sup)) { result.add(sup); collectSuperTypes(sup, result); }
        }
    }

    static class MemberKey {
        final String name;
        final Class<?>[] args;
        MemberKey(Method m) { this.name = m.getName(); this.args = m.getParameterTypes(); }
        public int hashCode() { return name.hashCode() + args.length; }
        public boolean equals(Object o) {
            if (o == this) return true;
            if (!(o instanceof MemberKey)) return false;
            MemberKey other = (MemberKey) o;
            if (!name.equals(other.name)) return false;
            if (args.length != other.args.length) return false;
            for (int i = 0; i < args.length; i++) if (args[i] != other.args[i]) return false;
            return true;
        }
    }
}
