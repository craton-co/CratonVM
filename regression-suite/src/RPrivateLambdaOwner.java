import java.util.function.Supplier;

/**
 * Regression for LambdaMetafactory handles to private synthetic bodies.
 *
 * Both parent and child compile a private lambda$new$0 method. The supplier
 * created by the parent must keep its resolved private owner even when its
 * captured receiver is a child instance.
 */
public class RPrivateLambdaOwner {
    private static int checks;

    private static void check(boolean condition, String message) {
        checks++;
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    public static void main(String[] args) {
        Child child = new Child();
        check(!child.parentValue(), "parent private lambda must stay bound to Parent");
        check(child.childValue(), "child private lambda must stay bound to Child");
        System.out.println("PASS RPrivateLambdaOwner (" + checks + " checks)");
    }

    private static class Parent {
        private final Supplier<Boolean> parent = () -> false;

        boolean parentValue() {
            return parent.get();
        }
    }

    private static final class Child extends Parent {
        private final Supplier<Boolean> child = () -> true;

        boolean childValue() {
            return child.get();
        }
    }
}
