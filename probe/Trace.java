package pkgtest;

import com.fasterxml.jackson.annotation.JsonProperty;
import com.fasterxml.jackson.databind.*;
import com.fasterxml.jackson.databind.cfg.MapperConfig;
import com.fasterxml.jackson.databind.introspect.*;

public class Trace {
    public static class A {
        @JsonProperty("y") private int y;
        public int getY() { return y; }
        public void setY(int v) { y = v; }
    }

    static class LoggingAI extends JacksonAnnotationIntrospector {
        @Override
        public String findImplicitPropertyName(AnnotatedMember m) {
            String r = super.findImplicitPropertyName(m);
            if (m instanceof AnnotatedMethod && ((AnnotatedMethod)m).getName().contains("etY"))
                System.out.println("findImplicitPropertyName(" + ((AnnotatedMethod)m).getName()
                    + " amId=" + System.identityHashCode(m) + ") = " + r);
            return r;
        }
        @Override
        public PropertyName findNameForSerialization(Annotated a) {
            PropertyName r = super.findNameForSerialization(a);
            if (a instanceof AnnotatedMethod && ((AnnotatedMethod)a).getName().contains("etY"))
                System.out.println("findNameForSerialization(" + ((AnnotatedMethod)a).getName()
                    + " amId=" + System.identityHashCode(a) + ") = " + r);
            return r;
        }
    }

    public static void main(String[] args) throws Exception {
        ObjectMapper m = new ObjectMapper();
        m.setAnnotationIntrospector(new LoggingAI());
        JavaType t = m.constructType(A.class);
        SerializationConfig cfg = m.getSerializationConfig();
        System.out.println("--- introspect begin ---");
        BeanDescription desc = cfg.introspect(t);
        System.out.println("--- introspect end ---");
        for (BeanPropertyDefinition pd : desc.findProperties())
            System.out.println("property '" + pd.getName() + "' hasGetter=" + pd.hasGetter());
    }
}
