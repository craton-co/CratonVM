public final class StackOverflowProbe {
    static int recurse(int n) {
        return 1 + recurse(n + 1);
    }

    public static void main(String[] args) {
        try {
            System.out.println(recurse(0));
            throw new AssertionError("recursion returned");
        } catch (StackOverflowError expected) {
            System.out.println("STACK_OVERFLOW_CAUGHT");
        }
    }
}
