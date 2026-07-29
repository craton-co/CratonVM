import java.lang.reflect.*;
import org.springframework.core.ResolvableType;

public class TypeVarProbe {
    static class Something {}
    static class SomethingImpl extends Something {}
    static abstract class Base<T extends Thing<S>, S extends Something> {
        T thing;
        S something;
    }
    static abstract class Thing<S extends Something> {}

    public static void main(String[] a) throws Exception {
        Field f = Base.class.getDeclaredField("something");
        Type g = f.getGenericType();
        System.out.println("genericType=" + g + " class=" + g.getClass().getName());
        if (g instanceof TypeVariable<?> tv) {
            System.out.println("  name=" + tv.getName());
            Type[] bounds = tv.getBounds();
            System.out.println("  bounds.length=" + bounds.length);
            for (Type b : bounds) System.out.println("    bound=" + b + " (" + b.getClass().getName() + ")");
            System.out.println("  genericDeclaration=" + tv.getGenericDeclaration());
        }
        ResolvableType rt = ResolvableType.forField(f, Base.class);
        System.out.println("ResolvableType=" + rt);
        System.out.println("  resolve()=" + rt.resolve());
        System.out.println("  toClass()=" + rt.toClass());
        Field ft = Base.class.getDeclaredField("thing");
        ResolvableType rt2 = ResolvableType.forField(ft, Base.class);
        System.out.println("thing ResolvableType=" + rt2 + " resolve=" + rt2.resolve());
    }
}
