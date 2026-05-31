package pkgtest;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.introspect.*;
import java.lang.reflect.*;
import java.util.*;

public class Chain {
    public static class A {
        @JsonProperty("y") private int y;
        public int getY() { return y; }
        public void setY(int v) { y = v; }
    }

    public static void main(String[] args) throws Exception {
        ObjectMapper m = new ObjectMapper();
        JavaType t = m.constructType(A.class);
        SerializationConfig cfg = m.getSerializationConfig();
        BeanDescription desc = cfg.introspect(t);

        for (BeanPropertyDefinition pd : desc.findProperties()) {
            System.out.println("property '" + pd.getName() + "' class=" + pd.getClass().getName());
            // reach into _getters linked list
            Field gf = findField(pd.getClass(), "_getters");
            if (gf == null) { System.out.println("  (no _getters field)"); continue; }
            gf.setAccessible(true);
            Object node = gf.get(pd);
            int i = 0;
            while (node != null) {
                Field vf = node.getClass().getDeclaredField("value");
                vf.setAccessible(true);
                Object am = vf.get(node);   // AnnotatedMethod
                Method underlying = ((AnnotatedMethod) am).getAnnotated();
                System.out.println("  getter[" + i + "] AnnotatedMethod id=" + System.identityHashCode(am)
                    + " reflectMethod id=" + System.identityHashCode(underlying)
                    + " declaringClass id=" + System.identityHashCode(((AnnotatedMethod) am).getDeclaringClass())
                    + " declaringClass=" + ((AnnotatedMethod) am).getDeclaringClass().getName());
                Field nf = node.getClass().getDeclaredField("next");
                nf.setAccessible(true);
                node = nf.get(node);
                i++;
            }
            System.out.println("  total getters in chain = " + i);
        }
    }

    static Field findField(Class<?> c, String name) {
        while (c != null) {
            try { return c.getDeclaredField(name); } catch (NoSuchFieldException e) { c = c.getSuperclass(); }
        }
        return null;
    }
}
