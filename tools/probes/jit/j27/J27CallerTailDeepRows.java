import java.lang.StackWalker;

public final class J27CallerTailDeepRows {
    private static final StackWalker WALKER = StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE);
    private static volatile Class<?> answer;

    static Class<?> caller() { return WALKER.getCallerClass(); }

    static void recurse(int depth) {
        if (depth == 0) { answer = caller(); return; }
        recurse(depth - 1);
    }

    public static void main(String[] args) {
        for (int i = 0; i < 10; i++) recurse(8);
        recurse(5000);
        System.out.println("fact caller " + answer.getName());
    }
}
