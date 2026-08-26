import java.util.function.Predicate;
public class LambdaSimpleNameProbe {
    interface Foo { void f(); }
    public static void main(String[] a) {
        Predicate<Throwable> p1 = t -> true;
        Predicate<Throwable> combined = p1.and(t -> false);
        Foo foo = () -> {};
        for (Object o : new Object[]{ p1, combined, foo }) {
            Class<?> c = o.getClass();
            System.out.println("getName       = " + c.getName());
            System.out.println("getSimpleName = " + c.getSimpleName());
            System.out.println("getCanonical  = " + c.getCanonicalName());
            System.out.println("---");
        }
    }
}
