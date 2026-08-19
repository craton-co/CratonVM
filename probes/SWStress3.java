package cratonvm;

import java.lang.StackWalker;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;

/**
 * Same call path and same heat as StackWalkerLog4jStressProbe, but the walk
 * collects the frame names it actually saw so a wrong answer prints the list
 * that produced it. (SWStress2's dump used a SEPARATE cold call chain, so it
 * showed an interpreted stack and proved nothing about the failing one.)
 */
public final class SWStress3 {
    private static final String OWNER = SWStress3.class.getName();
    private static final String EXPECTED = OWNER + "$LoggerFactory";
    private static final int DEPTH = 64;
    private static final int WALKS = 32;
    private static final StackWalker WALKER =
            StackWalker.getInstance(Set.of(StackWalker.Option.RETAIN_CLASS_REFERENCE));

    static String[] lastNames = new String[0];

    static final class Locator {
        static Class<?> getCallerClass(String fqcn, String pkg) {
            return WALKER.walk(stream -> {
                List<StackWalker.StackFrame> all = new ArrayList<>();
                stream.forEach(all::add);
                String[] names = new String[Math.min(all.size(), 8)];
                for (int i = 0; i < names.length; i++) {
                    names[i] = all.get(i).getClassName() + "." + all.get(i).getMethodName();
                }
                lastNames = names;
                int i = 0;
                while (i < all.size() && !all.get(i).getClassName().equals(fqcn)) i++;
                while (i < all.size() && all.get(i).getClassName().equals(fqcn)) i++;
                while (i < all.size() && !all.get(i).getClassName().startsWith(pkg)) i++;
                return i < all.size() ? all.get(i).getDeclaringClass() : null;
            });
        }
    }

    static final class LoggerFactory {
        static Class<?> resolveCaller() {
            return Locator.getCallerClass(Locator.class.getName(), OWNER);
        }
    }

    public static void main(String[] args) {
        int bad = 0;
        for (int i = 0; i < WALKS; i++) {
            Class<?> c = recurse(DEPTH);
            String got = (c == null) ? "null" : c.getName();
            if (!EXPECTED.equals(got)) {
                bad++;
                if (bad <= 2) {
                    StringBuilder sb = new StringBuilder();
                    for (String n : lastNames) sb.append(n).append(" | ");
                    System.out.println("walk#" + i + " got=" + got + " saw=" + sb);
                }
            }
        }
        System.out.println("bad=" + bad + "/" + WALKS);
        System.out.println(bad == 0 ? "STRESS3_OK" : "STRESS3_FAIL");
    }

    private static Class<?> recurse(int d) {
        if (d == 0) return LoggerFactory.resolveCaller();
        return recurse(d - 1);
    }
}
