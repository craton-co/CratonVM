import java.util.*;
import java.util.regex.*;
import java.util.stream.*;
import java.time.*;
import java.time.format.DateTimeFormatter;

/** SBR5 warmup lane: cold-start workload touching many JDK classes once. Prints per-phase ms and total. */
public class Sbr5WarmupCold {
    static long t;
    static void ph(String n) { long now = System.nanoTime(); System.out.println("phase " + n + " " + (now - t) / 1_000_000 + " ms"); t = System.nanoTime(); }
    public static void main(String[] a) throws Exception {
        long t00 = System.nanoTime(); t = t00;
        List<Integer> l = IntStream.range(0, 1000).boxed().collect(Collectors.toList());
        System.out.println(l.stream().filter(x -> x % 3 == 0).map(x -> "v" + x).collect(Collectors.joining(",")).length());
        Map<Integer,Integer> gm = l.stream().collect(Collectors.groupingBy(x -> x % 7, TreeMap::new, Collectors.summingInt(x -> x)));
        System.out.println(gm);
        ph("stream");
        System.out.println(String.format("%5d|%-8s|%08.3f|%x|%tY", 42, "ab", 3.14159, 255, new Date(0)));
        ph("format");
        System.out.println(LocalDate.of(2026, 10, 6).plusDays(100).format(DateTimeFormatter.ISO_DATE) + Duration.ofSeconds(3725) + ZonedDateTime.of(2026,1,1,0,0,0,0,ZoneId.of("Europe/Paris")));
        ph("time");
        Matcher m = Pattern.compile("([a-z]+)@([a-z]+)[.](com|org)").matcher("x foo@bar.com y baz@qux.org");
        int c = 0; while (m.find()) c += m.group(1).length();
        System.out.println(c + " " + "a,b;c d".split("[,; ]").length + " " + "hello".replaceAll("l+", "L"));
        ph("regex");
        int n = 0; for (java.nio.file.spi.FileSystemProvider p : java.nio.file.spi.FileSystemProvider.installedProviders()) n++;
        for (var x : ServiceLoader.load(java.nio.charset.spi.CharsetProvider.class)) n++;
        System.out.println(n);
        ph("serviceloader");
        String jar = a.length > 0 ? a[0] : System.getProperty("java.home") + "/lib/jrt-fs.jar";
        try (var cl = new java.net.URLClassLoader(new java.net.URL[]{new java.io.File(jar).toURI().toURL()})) {
            int k = 0;
            try (var jf = new java.util.jar.JarFile(jar)) {
                for (var e = jf.entries(); e.hasMoreElements() && k < 40;) { var en = e.nextElement(); String nm = en.getName(); if (nm.endsWith(".class") && !nm.contains("module-info")) { try { Class.forName(nm.substring(0, nm.length()-6).replace('/', '.'), false, cl); k++; } catch (Throwable ex) {} } }
            }
            System.out.println("loaded " + k);
        }
        ph("jarload");
        Object o = Class.forName("java.util.concurrent.ConcurrentSkipListMap").getDeclaredConstructor().newInstance();
        java.lang.reflect.Method mm = o.getClass().getMethod("size"); System.out.println(mm.invoke(o));
        java.util.concurrent.ExecutorService ex = java.util.concurrent.Executors.newFixedThreadPool(2);
        System.out.println(ex.submit(() -> 5).get()); ex.shutdown();
        System.out.println(java.util.concurrent.CompletableFuture.supplyAsync(() -> 9).thenApply(x -> x + 1).get());
        ph("reflect+exec");
        System.out.println("TOTAL " + (System.nanoTime() - t00) / 1_000_000 + " ms");
    }
}
