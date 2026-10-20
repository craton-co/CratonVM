// The interpreted phase of a real run: class loading plus the reflective and
// OO work a framework does before anything tiers up. Everything here executes
// once or a handful of times, so it never reaches the JIT -- which is the
// population the interpreter changes actually serve.
import java.lang.reflect.*;
public class ClassLoadShape {
    static final String[] CLASSES = {
        "java.util.ArrayList","java.util.LinkedList","java.util.HashMap","java.util.TreeMap",
        "java.util.LinkedHashMap","java.util.HashSet","java.util.TreeSet","java.util.ArrayDeque",
        "java.util.concurrent.ConcurrentHashMap","java.util.concurrent.CopyOnWriteArrayList",
        "java.util.concurrent.atomic.AtomicInteger","java.util.concurrent.atomic.AtomicLong",
        "java.io.File","java.io.BufferedReader","java.io.InputStreamReader","java.io.StringWriter",
        "java.io.ByteArrayOutputStream","java.io.ObjectOutputStream","java.io.DataOutputStream",
        "java.nio.ByteBuffer","java.nio.CharBuffer","java.nio.charset.Charset",
        "java.text.SimpleDateFormat","java.text.DecimalFormat","java.text.NumberFormat",
        "java.time.LocalDate","java.time.LocalDateTime","java.time.Duration","java.time.Instant",
        "java.math.BigInteger","java.math.BigDecimal","java.util.regex.Pattern",
        "java.util.stream.Collectors","java.util.Optional","java.util.UUID","java.util.Random",
        "java.util.StringJoiner","java.util.Scanner","java.util.BitSet","java.util.Currency",
    };
    public static void main(String[] a) throws Exception {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 40;
        long t0 = System.nanoTime();
        int sink = 0;
        for (int r = 0; r < rounds; r++) {
            for (String cn : CLASSES) {
                Class<?> c = Class.forName(cn);
                // The reflective introspection a framework does per class:
                // walk the members, touch names and modifiers. All interpreted.
                Method[] ms = c.getDeclaredMethods();
                sink += ms.length;
                for (Method m : ms) {
                    sink += m.getName().length() + m.getParameterCount() + m.getModifiers();
                }
                Field[] fs = c.getDeclaredFields();
                sink += fs.length;
                for (Field f : fs) sink += f.getName().length() + f.getModifiers();
                Constructor<?>[] cs = c.getDeclaredConstructors();
                sink += cs.length;
                for (Constructor<?> k : cs) sink += k.getParameterCount();
                Class<?>[] is = c.getInterfaces();
                sink += is.length;
                for (Class<?> i : is) sink += i.getName().length();
                Class<?> sup = c.getSuperclass();
                if (sup != null) sink += sup.getName().length();
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("classload+reflect: " + ms + " ms  sink=" + sink);
    }
}
