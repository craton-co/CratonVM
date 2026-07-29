import java.lang.reflect.*;

public class TypeVarProbe2 {
    static class Something {}
    static abstract class Thing<S extends Something> {}
    static abstract class Base<T extends Thing<S>, S extends Something> { T thing; S something; }
    // control: same two params, but the FIRST bound has no nested type argument
    static abstract class Base2<T extends Thing, S extends Something> { T thing; S something; }
    // control: single param with a nested type argument
    static abstract class Base3<T extends Thing<Something>> { T thing; }

    static void dump(Class<?> c) {
        System.out.println("-- " + c.getSimpleName());
        for (TypeVariable<?> tv : c.getTypeParameters()) {
            StringBuilder sb = new StringBuilder();
            for (Type b : tv.getBounds()) sb.append(b).append(" ");
            System.out.println("   " + tv.getName() + " bounds=[ " + sb + "]");
        }
    }

    public static void main(String[] a) {
        dump(Base.class);
        dump(Base2.class);
        dump(Base3.class);
        dump(Thing.class);
    }
}
