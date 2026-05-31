package pkgtest;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.introspect.*;
import java.lang.reflect.*;
import java.util.*;

public class Chain2 {
    public static class A {
        @JsonProperty("y") private int y;
        public int getY() { return y; }
        public void setY(int v) { y = v; }
    }

    public static void main(String[] args) throws Exception {
        ObjectMapper m = new ObjectMapper();
        JavaType t = m.constructType(A.class);
        SerializationConfig cfg = m.getSerializationConfig();
        AnnotatedClass ac = AnnotatedClassResolver.resolve(cfg, t, cfg);

        System.out.println("== memberMethods AnnotatedMethod ids ==");
        for (AnnotatedMethod am : ac.memberMethods()) {
            if (am.getName().equals("getY"))
                System.out.println("  memberMethod getY AnnotatedMethod id=" + System.identityHashCode(am)
                    + " reflectMethod id=" + System.identityHashCode(am.getAnnotated()));
        }

        // also: AnnotatedMethodMap.find via MemberKey
        Method real = A.class.getDeclaredMethod("getY");
        AnnotatedMethod found = ac.findMethod("getY", new Class<?>[0]);
        System.out.println("  findMethod(getY) id=" + System.identityHashCode(found));

        BeanDescription desc = cfg.introspect(t);
        AnnotatedClass aci = desc.getClassInfo();
        System.out.println("== desc.getClassInfo().memberMethods getY ==");
        for (AnnotatedMethod am : aci.memberMethods()) {
            if (am.getName().equals("getY"))
                System.out.println("  getY AnnotatedMethod id=" + System.identityHashCode(am)
                    + " reflectMethod id=" + System.identityHashCode(am.getAnnotated()));
        }
        for (BeanPropertyDefinition pd : desc.findProperties()) {
            Field gf = findField(pd.getClass(), "_getters");
            gf.setAccessible(true);
            Object node = gf.get(pd);
            int i = 0;
            while (node != null) {
                Field vf = node.getClass().getDeclaredField("value");
                vf.setAccessible(true);
                Object am = vf.get(node);
                System.out.println("  _getters[" + i + "] AnnotatedMethod id=" + System.identityHashCode(am)
                    + " reflectMethod id=" + System.identityHashCode(((AnnotatedMethod) am).getAnnotated()));
                Field nf = node.getClass().getDeclaredField("next");
                nf.setAccessible(true);
                node = nf.get(node);
                i++;
            }
        }
    }

    static Field findField(Class<?> c, String name) {
        while (c != null) {
            try { return c.getDeclaredField(name); } catch (NoSuchFieldException e) { c = c.getSuperclass(); }
        }
        return null;
    }
}
