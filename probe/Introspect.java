package pkgtest;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.introspect.*;
import java.lang.reflect.Method;
import java.util.*;

public class Introspect {
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

        System.out.println("== memberMethods ==");
        int getYcount = 0;
        for (AnnotatedMethod am : ac.memberMethods()) {
            Method underlying = am.getAnnotated();
            String tag = am.getName().equals("getY")
                ? "  <-- AnnotatedMethod id=" + System.identityHashCode(am)
                  + " reflectMethod id=" + System.identityHashCode(underlying)
                : "";
            System.out.println("  " + am.getName() + "(" + am.getParameterCount() + ")" + tag);
            if (am.getName().equals("getY")) getYcount++;
        }
        System.out.println("memberMethods getY count = " + getYcount);

        // Now the full POJO properties collection
        System.out.println("== BeanDescription properties ==");
        BeanDescription desc = cfg.introspect(t);
        for (BeanPropertyDefinition pd : desc.findProperties()) {
            System.out.println("  property '" + pd.getName() + "' hasGetter=" + pd.hasGetter());
        }
    }
}
