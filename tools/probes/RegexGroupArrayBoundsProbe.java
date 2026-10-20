import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Checks the bulk-group read's successful and declined bounds paths. */
public class RegexGroupArrayBoundsProbe {
    public static void main(String[] args) throws Exception {
        Field groups = Matcher.class.getDeclaredField("groups");
        groups.setAccessible(true);
        Matcher matcher = Pattern.compile("(\\d+)").matcher("x12y");
        for (int i = 0; i < 5000; i++) {
            matcher.reset();
            if (!matcher.find() || !matcher.group(1).equals("12")) throw new AssertionError();
        }
        for (int length : new int[]{0, 1, 2, 3, 4, 20}) {
            for (int group : new int[]{-1, 0, 1, 2}) {
                matcher = Pattern.compile("(\\d+)").matcher("x12y");
                matcher.find();
                groups.set(matcher, Arrays.copyOf((int[]) groups.get(matcher), length));
                System.out.print("length=" + length + " group=" + group + " ");
                try {
                    System.out.println("value=" + matcher.group(group));
                } catch (RuntimeException error) {
                    System.out.println(error.getClass().getName() + ": " + error.getMessage());
                }
            }
        }
        matcher = Pattern.compile("(\\d+)").matcher("x12y");
        try { matcher.group(-1); } catch (RuntimeException error) {
            System.out.println("no-match " + error.getClass().getName() + ": " + error.getMessage());
        }
    }
}
