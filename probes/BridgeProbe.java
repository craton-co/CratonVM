import java.io.Serializable;
import java.lang.reflect.*;
import java.beans.*;
import java.util.*;

public class BridgeProbe {
    interface Entity<T extends Serializable> { T getId(); void setId(T id); }
    abstract static class BaseEntity<T extends Number> implements Entity<T> {
        private T id;
        @Override public T getId() { return this.id; }
        @Override public void setId(T id) { this.id = id; }
    }
    static class Person extends BaseEntity<Long> {}
    static class PersonWithOverriddenGetter extends BaseEntity<Long> {
        @Override public Long getId() { return super.getId(); }
    }
    static class PersonWithOverloadedSetter extends BaseEntity<Long> {
        public void setId(int id) { setId(Long.valueOf(id)); }
    }

    static void dump(Class<?> c) {
        System.out.println("== " + c.getSimpleName());
        Method[] ms = c.getDeclaredMethods();
        Arrays.sort(ms, Comparator.comparing(Method::toString));
        for (Method m : ms) {
            System.out.println("   " + m.getName() + " params=" + Arrays.toString(m.getParameterTypes())
                    + " ret=" + m.getReturnType().getSimpleName()
                    + " bridge=" + m.isBridge() + " synth=" + m.isSynthetic()
                    + " genRet=" + m.getGenericReturnType()
                    + " genParams=" + Arrays.toString(m.getGenericParameterTypes()));
        }
        try {
            BeanInfo bi = Introspector.getBeanInfo(c);
            for (PropertyDescriptor pd : bi.getPropertyDescriptors()) {
                if (pd.getName().equals("class")) continue;
                System.out.println("   PD " + pd.getName() + " type=" + (pd.getPropertyType()==null?"null":pd.getPropertyType().getSimpleName())
                        + " read=" + (pd.getReadMethod()==null?"null":pd.getReadMethod().getReturnType().getSimpleName())
                        + " write=" + (pd.getWriteMethod()==null?"null":Arrays.toString(pd.getWriteMethod().getParameterTypes())));
            }
        } catch (Exception e) { System.out.println("   introspector: " + e); }
    }

    public static void main(String[] a) {
        dump(BaseEntity.class);
        dump(Person.class);
        dump(PersonWithOverriddenGetter.class);
        dump(PersonWithOverloadedSetter.class);
    }
}
