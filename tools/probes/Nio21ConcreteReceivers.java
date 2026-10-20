// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Modifier;
import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.spi.FileSystemProvider;
import java.util.Arrays;
import java.util.Iterator;
import java.util.Optional;
import java.util.Spliterator;
import java.util.stream.Collectors;
import java.util.stream.Stream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

/**
 * nio21 wave 11, lane L1: every receiver a native hands back must be an instance of a class
 * `new` could produce (JVMS 6.5: `new` on an INTERFACE or an ABSTRACT class is an
 * `InstantiationError`). The VM's own census (`CRATONVM_DBG=exit-census`, the line
 * `JVMS 6.5 uninstantiable-receiver census`) named four on the Spring Boot sample:
 * `java/util/stream/Stream` (the `StackWalker.walk` stream, both modes) and, under
 * `--compatible`, `VarHandle`, `MethodHandle` and `FileSystem`. Page:
 * docs/known-issues/jdk-only/r21-natives-hand-back-uninstantiable-receivers-20261008.md.
 *
 * Conformance only: every line is a fact. Each object row prints its runtime class name
 * (a hidden class's `/0x...` suffix stripped), whether that class is an interface or abstract
 * (HotSpot: always `false false`), and what the object does.
 *
 *   javac -d out tools/probes/Nio21ConcreteReceivers.java
 *   java -cp out Nio21ConcreteReceivers > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21ConcreteReceivers > jo.txt
 *   cratonvm --compatible -cp out Nio21ConcreteReceivers > co.txt
 *   CRATONVM_DBG=exit-census cratonvm --compatible -cp out Nio21ConcreteReceivers 2>&1 \
 *       | grep 'uninstantiable-receiver census'     # must print nothing
 *
 * hs, jo and co must be identical. -Dnio21.rows=a,b selects rows (walk, vh, vhcast, mh, fs);
 * -Dnio21.rows=bench prints `walk-skip-findFirst <N> ns/op` (3 reps, -Dnio21.n=20000).
 * The `fs` rows name the platform's classes (`sun.nio.fs.LinuxFileSystem` on Linux), so
 * compare against HotSpot on the same OS.
 */
public class Nio21ConcreteReceivers {
    static final String ROWS = System.getProperty("nio21.rows", "all");

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    static void say(String s) {
        System.out.println(s);
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    /** The runtime class name with a hidden class's `/0x...` suffix stripped. */
    static String name(Class<?> c) {
        String n = c.getName();
        int slash = n.indexOf('/');
        return slash < 0 ? n : n.substring(0, slash);
    }

    /** `<class> iface=<b> abstract=<b>` for the runtime class of `o`. */
    static String shape(Object o) {
        if (o == null) return "null";
        Class<?> c = o.getClass();
        return name(c) + " iface=" + c.isInterface() + " abstract=" + Modifier.isAbstract(c.getModifiers());
    }

    // ---------------------------------------------------------------- walk

    static final StackWalker WALKER = StackWalker.getInstance();

    static String firstMethod(Stream<StackWalker.StackFrame> s) {
        return s.map(StackWalker.StackFrame::getMethodName).findFirst().orElse("none");
    }

    static void walkRows() {
        say("walk-shape " + WALKER.walk(s -> shape(s)));
        say("walk-is-stream " + WALKER.walk(s -> (Object) s instanceof Stream));
        say("walk-first " + WALKER.walk(Nio21ConcreteReceivers::firstMethod));
        say("walk-derived-shape " + WALKER.walk(s -> shape(s.map(StackWalker.StackFrame::getMethodName))));
        say("walk-skip " + WALKER.walk(s -> s.skip(1).map(StackWalker.StackFrame::getMethodName)
                .findFirst().orElse("none")));
        say("walk-limit " + WALKER.walk(s -> s.limit(2).map(StackWalker.StackFrame::getMethodName)
                .collect(Collectors.joining(","))));
        say("walk-spliterator " + WALKER.walk(s -> {
            Spliterator<StackWalker.StackFrame> sp = s.spliterator();
            return sp.characteristics() + " " + sp.getExactSizeIfKnown()
                    + " " + (sp.trySplit() == null);
        }));
        try {
            say("walk-reuse " + WALKER.walk(s -> {
                s.count();
                return s.count();
            }));
        } catch (IllegalStateException e) {
            say("walk-reuse " + ex(e));
        }
        Stream<StackWalker.StackFrame> escaped = WALKER.walk(s -> s);
        say("walk-escaped-shape " + shape(escaped));
        try {
            say("walk-escaped-count " + escaped.count());
        } catch (IllegalStateException e) {
            say("walk-escaped-count " + ex(e));
        }
        Stream<String> derived = WALKER.walk(s -> s.map(StackWalker.StackFrame::getMethodName));
        try {
            say("walk-escaped-derived " + derived.findFirst());
        } catch (IllegalStateException e) {
            say("walk-escaped-derived " + ex(e));
        }
        Iterator<StackWalker.StackFrame> it = WALKER.walk(s -> s.iterator());
        try {
            say("walk-escaped-iterator " + it.hasNext());
        } catch (IllegalStateException e) {
            say("walk-escaped-iterator " + ex(e));
        }
        Optional<String> inside = WALKER.walk(s -> {
            Stream<String> d = s.map(StackWalker.StackFrame::getMethodName);
            return d.filter(m -> m.equals("walkRows")).findAny();
        });
        say("walk-inside-filter " + inside.orElse("none"));
    }

    // ---------------------------------------------------------------- vh

    static class Holder {
        int i = 7;
        String s = "s0";
        static long L = 11L;
    }

    static void vhRows() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle fi = lk.findVarHandle(Holder.class, "i", int.class);
        Holder h = new Holder();
        fi.set(h, 9);
        say("vh-instance-int " + shape(fi) + " get=" + (int) fi.get(h)
                + " cas=" + fi.compareAndSet(h, 9, 10) + " now=" + h.i);
        VarHandle fs = lk.findVarHandle(Holder.class, "s", String.class);
        say("vh-instance-ref " + shape(fs) + " gas=" + fs.getAndSet(h, "s1") + " now=" + h.s);
        VarHandle st = lk.findStaticVarHandle(Holder.class, "L", long.class);
        say("vh-static-long " + shape(st) + " gaa=" + (long) st.getAndAdd(5L) + " now=" + Holder.L);
        VarHandle ae = MethodHandles.arrayElementVarHandle(int[].class);
        int[] a = {1, 2, 3};
        ae.setVolatile(a, 1, 20);
        say("vh-array-int " + shape(ae) + " get=" + (int) ae.get(a, 1));
        VarHandle ar = MethodHandles.arrayElementVarHandle(String[].class);
        say("vh-array-ref " + shape(ar));
        say("vh-is-varhandle " + ((Object) fi instanceof VarHandle) + " " + fi.varType()
                + " " + fi.coordinateTypes());
    }

    // ---------------------------------------------------------------- vhcast

    static class Node {
        public String toString() {
            return "node";
        }
    }

    static class Cell {
        Node n = new Node();
        static Node S = new Node();
    }

    interface Op {
        Object run() throws Throwable;
    }

    static void row(String name, Op op) {
        try {
            say("vhcast-" + name + " " + op.run());
        } catch (Throwable t) {
            say("vhcast-" + name + " " + ex(t));
        }
    }

    /**
     * nio21 wave 12 L3: every compare-and-set / compare-and-exchange access mode casts BOTH the
     * expectation and the new value to the variable's type (HotSpot: `fieldType.cast(expected)`
     * before `fieldType.cast(value)`), for an instance field, a static field and an array
     * element, through an erased `Object` call site. Each row then prints the variable, which a
     * refused access must not have changed.
     */
    static void vhCastRows() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle f = lk.findVarHandle(Cell.class, "n", Node.class);
        VarHandle s = lk.findStaticVarHandle(Cell.class, "S", Node.class);
        VarHandle a = MethodHandles.arrayElementVarHandle(Node[].class);
        Cell c = new Cell();
        Node[] arr = {new Node()};
        Object str = "str";
        Object nul = null;
        Object cur = c.n;
        row("field-cas-exp", () -> f.compareAndSet(c, str, nul) + " " + c.n);
        row("field-cas-new", () -> f.compareAndSet(c, cur, str) + " " + c.n);
        row("field-cas-both", () -> f.compareAndSet(c, str, Integer.valueOf(1)) + " " + c.n);
        row("field-wcas-exp", () -> f.weakCompareAndSet(c, str, nul) + " " + c.n);
        row("field-wcasp-exp", () -> f.weakCompareAndSetPlain(c, str, nul) + " " + c.n);
        row("field-wcasa-exp", () -> f.weakCompareAndSetAcquire(c, str, nul) + " " + c.n);
        row("field-wcasr-exp", () -> f.weakCompareAndSetRelease(c, str, nul) + " " + c.n);
        row("field-cae-exp", () -> (Object) f.compareAndExchange(c, str, nul) + " " + c.n);
        row("field-caea-exp", () -> (Object) f.compareAndExchangeAcquire(c, str, nul) + " " + c.n);
        row("field-caer-exp", () -> (Object) f.compareAndExchangeRelease(c, str, nul) + " " + c.n);
        row("field-cae-new", () -> (Object) f.compareAndExchange(c, cur, str) + " " + c.n);
        row("field-cas-ok", () -> f.compareAndSet(c, cur, cur) + " " + c.n);
        Object scur = Cell.S;
        row("static-cas-exp", () -> s.compareAndSet(str, nul) + " " + Cell.S);
        row("static-cas-new", () -> s.compareAndSet(scur, str) + " " + Cell.S);
        row("static-cae-exp", () -> (Object) s.compareAndExchange(str, nul) + " " + Cell.S);
        row("static-wcas-exp", () -> s.weakCompareAndSetPlain(str, nul) + " " + Cell.S);
        row("array-cas-exp", () -> a.compareAndSet(arr, 0, str, nul) + " " + arr[0]);
        row("array-cas-new", () -> a.compareAndSet(arr, 0, (Object) arr[0], str) + " " + arr[0]);
        row("array-cae-exp", () -> (Object) a.compareAndExchange(arr, 0, str, nul) + " " + arr[0]);
        row("field-gas-new", () -> (Object) f.getAndSet(c, str) + " " + c.n);
        row("field-set-new", () -> {
            f.set(c, str);
            return c.n;
        });
    }

    // ---------------------------------------------------------------- mh

    static int twice(int x) {
        return 2 * x;
    }

    static void mhRows() throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodHandle virt = lk.findVirtual(String.class, "length", MethodType.methodType(int.class));
        say("mh-virtual " + shape(virt) + " r=" + (int) virt.invokeExact("abcd"));
        MethodHandle stat = lk.findStatic(Nio21ConcreteReceivers.class, "twice",
                MethodType.methodType(int.class, int.class));
        say("mh-static " + shape(stat) + " r=" + (int) stat.invokeExact(21));
        MethodHandle ctor = lk.findConstructor(StringBuilder.class,
                MethodType.methodType(void.class, String.class));
        say("mh-constructor " + shape(ctor) + " r=" + ctor.invoke("x"));
        MethodHandle bound = virt.bindTo("hello");
        say("mh-bound " + shape(bound) + " r=" + (int) bound.invokeExact());
        say("mh-is-methodhandle " + ((Object) stat instanceof MethodHandle) + " " + stat.type());
    }

    // ---------------------------------------------------------------- fs

    static void fsRows() throws Exception {
        FileSystem def = FileSystems.getDefault();
        say("fs-default " + shape(def) + " provider=" + shape(def.provider())
                + " same=" + (def == FileSystems.getDefault()) + " open=" + def.isOpen());
        // nio21 wave 14 L3: one default filesystem, whichever door names it.
        say("fs-identity " + (def == Path.of("a").getFileSystem())
                + " " + (def.provider() == Path.of("a").getFileSystem().provider())
                + " " + (def == FileSystems.getFileSystem(URI.create("file:///")))
                + " " + (FileSystemProvider.installedProviders().get(0) == def.provider()));
        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        say("fs-jrt " + shape(jrt) + " object=" + Files.exists(jrt.getPath("modules", "java.base",
                "java/lang/Object.class")));
        say("fs-jrt-provider " + shape(jrt.provider()) + " scheme=" + jrt.provider().getScheme());
        Path zip = Files.createTempFile("nio21cr", ".zip");
        try {
            try (ZipOutputStream zo = new ZipOutputStream(Files.newOutputStream(zip))) {
                zo.putNextEntry(new ZipEntry("a.txt"));
                zo.write("hi".getBytes());
                zo.closeEntry();
            }
            try (FileSystem zfs = FileSystems.newFileSystem(zip)) {
                say("fs-zip " + shape(zfs) + " a=" + new String(Files.readAllBytes(zfs.getPath("a.txt"))));
                say("fs-zip-provider " + shape(zfs.provider()) + " scheme=" + zfs.provider().getScheme());
            }
        } finally {
            Files.deleteIfExists(zip);
        }
    }

    // ---------------------------------------------------------------- bench

    /** Timing, only when asked (`-Dnio21.rows=bench`): never part of "all". */
    static void benchRows() {
        int n = Integer.getInteger("nio21.n", 20_000);
        long sink = 0;
        for (int rep = 0; rep < 3; rep++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                sink += WALKER.walk(s -> s.skip(1).findFirst()).map(f -> f.getLineNumber()).orElse(0);
            }
            long t1 = System.nanoTime();
            say("walk-skip-findFirst " + ((t1 - t0) / n) + " ns/op");
        }
        if (sink == 42) say("");
    }

    public static void main(String[] args) throws Throwable {
        if (Arrays.asList(ROWS.split(",")).contains("bench")) benchRows();
        if (on("walk")) walkRows();
        if (on("vh")) vhRows();
        if (on("vhcast")) vhCastRows();
        if (on("mh")) mhRows();
        if (on("fs")) fsRows();
        say("done");
    }
}
