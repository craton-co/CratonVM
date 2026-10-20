/**
 * gci i32/mf (2026-10-15): the {@code Metaspace} figure against HotSpot 25
 * {@code -Xshare:off} (page
 * {@code gci-i31hp-metaspace-figure-is-not-hotspots-metaspace-used-20261015}).
 * stdout is deterministic ({@code mf32 <mode> done}); the figures are on
 * stderr, one line each:
 *
 * <ul>
 *   <li>{@code [mf32] base classes=<loaded> metaspace=<used> perclass=<used/loaded> ccs=<used>}:
 *       the {@code Metaspace} and {@code Compressed Class Space} pools' used
 *       bytes and the {@code ClassLoadingMXBean} loaded count, after the
 *       management classes are loaded.</li>
 *   <li>{@code load} mode only, after {@code Class.forName(name, true, ...)} of
 *       a fixed list of JDK classes:
 *       {@code [mf32] load dclasses=<delta> dmetaspace=<delta> perclass=<dmetaspace/dclasses>}.</li>
 * </ul>
 *
 * Modes: {@code small} (the base line only), {@code load} (base + load).
 */
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;

public class GciMf32MetaspaceFigureProbe {
    static final String[] NAMES = {
        "java.util.TreeMap", "java.util.TreeSet", "java.util.LinkedList", "java.util.ArrayDeque",
        "java.util.PriorityQueue", "java.util.BitSet", "java.util.Stack", "java.util.Vector",
        "java.util.IdentityHashMap", "java.util.WeakHashMap", "java.util.EnumMap", "java.util.Calendar",
        "java.util.GregorianCalendar", "java.util.Scanner", "java.util.StringJoiner", "java.util.Timer",
        "java.util.UUID", "java.util.Base64", "java.util.Currency", "java.util.zip.CRC32",
        "java.util.zip.Adler32", "java.util.zip.Deflater", "java.util.zip.Inflater",
        "java.util.concurrent.ConcurrentSkipListMap", "java.util.concurrent.CopyOnWriteArrayList",
        "java.util.concurrent.LinkedBlockingQueue", "java.util.concurrent.ArrayBlockingQueue",
        "java.util.concurrent.Semaphore", "java.util.concurrent.CountDownLatch",
        "java.util.concurrent.CyclicBarrier", "java.util.concurrent.Exchanger",
        "java.util.concurrent.Phaser", "java.util.concurrent.atomic.LongAdder",
        "java.util.concurrent.atomic.AtomicLongArray", "java.util.concurrent.locks.StampedLock",
        "java.util.concurrent.locks.ReentrantReadWriteLock", "java.util.regex.Pattern",
        "java.text.DecimalFormat", "java.text.SimpleDateFormat", "java.text.MessageFormat",
        "java.math.BigDecimal", "java.math.BigInteger", "java.math.MathContext",
        "java.time.LocalDate", "java.time.LocalDateTime", "java.time.Duration", "java.time.Instant",
        "java.time.ZonedDateTime", "java.time.Period", "java.time.Year", "java.time.YearMonth",
        "java.time.format.DateTimeFormatter", "java.io.StringReader", "java.io.StringWriter",
        "java.io.PushbackReader", "java.io.LineNumberReader", "java.io.PipedInputStream",
        "java.io.SequenceInputStream", "java.io.DataInputStream", "java.io.ObjectOutputStream",
        "java.nio.CharBuffer", "java.nio.LongBuffer", "java.nio.file.Files",
    };

    static MemoryPoolMXBean pool(String name) {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals(name)) {
                return p;
            }
        }
        return null;
    }

    static long used(MemoryPoolMXBean p) {
        return p == null ? -1 : p.getUsage().getUsed();
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "small";
        ClassLoadingMXBean cl = ManagementFactory.getClassLoadingMXBean();
        MemoryPoolMXBean meta = pool("Metaspace");
        MemoryPoolMXBean ccs = pool("Compressed Class Space");
        // Warm the sampling path once so its own classes are in the base.
        used(meta);
        used(ccs);
        cl.getLoadedClassCount();
        long classes = cl.getLoadedClassCount();
        long ms = used(meta);
        System.err.println("[mf32] base classes=" + classes + " metaspace=" + ms
                + " perclass=" + (classes > 0 ? ms / classes : -1) + " ccs=" + used(ccs));
        if (mode.equals("load")) {
            ClassLoader loader = GciMf32MetaspaceFigureProbe.class.getClassLoader();
            int missing = 0;
            for (String n : NAMES) {
                try {
                    Class.forName(n, true, loader);
                } catch (Throwable t) {
                    missing++;
                }
            }
            long classes2 = cl.getLoadedClassCount();
            long ms2 = used(meta);
            long dc = classes2 - classes;
            long dm = ms2 - ms;
            System.err.println("[mf32] load dclasses=" + dc + " dmetaspace=" + dm
                    + " perclass=" + (dc > 0 ? dm / dc : -1) + " missing=" + missing);
        }
        System.out.println("mf32 " + mode + " done");
    }
}
