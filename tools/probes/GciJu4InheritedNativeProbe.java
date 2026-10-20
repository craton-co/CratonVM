// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Properties;
import java.util.TreeSet;
import java.util.concurrent.Callable;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;

/**
 * gci i4/ju: inherited JDK methods called on an APPLICATION subclass receiver,
 * on the call sites' first (cold) execution and then warm.
 *
 * The i4/ju fix makes the interpreter's cold superclass walk
 * ({@code invoke.rs} {@code try_stackless_invoke} step 1, which starts at the
 * receiver's class) admit an ancestor's registered native only as the warm
 * populate walk does, through {@code resolve_dispatch}. A subclass receiver
 * that does not declare the method is exactly the shape that walk serves, so
 * every section below reaches it on its first call. Each section builds fresh
 * objects and returns one summary string; the probe prints the cold summary
 * and how many of the warm repetitions returned the identical string. On
 * HotSpot both are deterministic and {@code warmSame} equals the repetition
 * count; a VM whose cold and warm doors disagree prints a different cold
 * summary, a short {@code warmSame}, or both.
 *
 * Usage: {@code GciJu4InheritedNativeProbe [reps]} (default 200; the two
 * thread sections run reps/10, at least 1). Stdout is deterministic.
 */
public class GciJu4InheritedNativeProbe {
    static final class Hm extends HashMap<String, Integer> {
        private static final long serialVersionUID = 1L;
    }

    static final class Lhm extends LinkedHashMap<String, Integer> {
        private static final long serialVersionUID = 1L;
    }

    static final class Al extends ArrayList<Integer> {
        private static final long serialVersionUID = 1L;
    }

    static final class Dq extends ArrayDeque<Integer> {
        private static final long serialVersionUID = 1L;
    }

    static final class Props extends Properties {
        private static final long serialVersionUID = 1L;
    }

    static final class Ex extends Exception {
        private static final long serialVersionUID = 1L;

        Ex(String message) {
            super(message);
        }
    }

    static final class Th extends Thread {
        private final StringBuilder out;

        Th(String name, StringBuilder out) {
            super(name);
            this.out = out;
        }

        @Override
        public void run() {
            out.append("ran:").append(getName());
        }
    }

    static final class Cl extends ClassLoader {
        Cl() {
            super();
        }
    }

    static final class Tpe extends ThreadPoolExecutor {
        Tpe() {
            super(1, 1, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
        }
    }

    interface Section {
        String run() throws Exception;
    }

    static String hashMap() {
        Hm m = new Hm();
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        m.remove("b");
        m.putIfAbsent("d", 4);
        m.merge("a", 10, Integer::sum);
        return "size=" + m.size() + " a=" + m.get("a") + " hasB=" + m.containsKey("b")
                + " keys=" + new TreeSet<String>(m.keySet()) + " z=" + m.getOrDefault("z", -1);
    }

    static String linkedHashMap() {
        Lhm m = new Lhm();
        m.put("c", 3);
        m.put("a", 1);
        m.put("b", 2);
        Integer a = m.get("a");
        m.remove("c");
        m.put("d", 4);
        return "a=" + a + " keys=" + m.keySet() + " values=" + m.values();
    }

    static String arrayList() {
        Al l = new Al();
        l.add(3);
        l.add(1);
        l.add(2);
        l.add(0, 9);
        l.remove(Integer.valueOf(1));
        return l + " idx2=" + l.indexOf(2) + " has3=" + l.contains(3) + " size=" + l.size();
    }

    static String arrayDeque() {
        Dq d = new Dq();
        d.addFirst(1);
        d.addLast(2);
        d.addFirst(0);
        Integer first = d.pollFirst();
        return "first=" + first + " last=" + d.peekLast() + " size=" + d.size() + " " + d;
    }

    static String properties() {
        Props p = new Props();
        p.setProperty("k", "v");
        p.setProperty("a", "b");
        return "k=" + p.getProperty("k") + " x=" + p.getProperty("x", "dflt")
                + " names=" + new TreeSet<String>(p.stringPropertyNames());
    }

    static String exception() {
        Ex e = new Ex("boom");
        return "msg=" + e.getMessage() + " str=" + e + " loc=" + e.getLocalizedMessage()
                + " cause=" + e.getCause();
    }

    static String thread() throws InterruptedException {
        StringBuilder sb = new StringBuilder();
        Th t = new Th("ju4-th", sb);
        String before = t.getName();
        t.setName("ju4-renamed");
        boolean daemonBefore = t.isDaemon();
        t.setDaemon(true);
        t.start();
        t.join();
        return "before=" + before + " daemon=" + daemonBefore + "/" + t.isDaemon() + " out=" + sb
                + " alive=" + t.isAlive() + " state=" + t.getState();
    }

    static String classLoader() throws ClassNotFoundException {
        Cl c = new Cl();
        return "parentIsSystem=" + (c.getParent() == ClassLoader.getSystemClassLoader())
                + " string=" + (c.loadClass("java.lang.String") == String.class);
    }

    static String executor() throws Exception {
        Tpe x = new Tpe();
        Callable<Integer> task = () -> 42;
        Future<Integer> f = x.submit(task);
        Integer got = f.get();
        x.shutdown();
        boolean terminated = x.awaitTermination(30, TimeUnit.SECONDS);
        return "got=" + got + " shutdown=" + x.isShutdown() + " terminated=" + terminated + "/"
                + x.isTerminated() + " completed=" + x.getCompletedTaskCount();
    }

    static String guarded(Section s) {
        try {
            return s.run();
        } catch (Throwable t) {
            return "threw " + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void report(String name, Section s, int reps) {
        String cold = guarded(s);
        int same = 0;
        for (int i = 0; i < reps; i++) {
            if (cold.equals(guarded(s))) {
                same++;
            }
        }
        System.out.println(name + " cold=" + cold + " warmSame=" + same + "/" + reps);
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int threadReps = Math.max(1, reps / 10);
        report("hashmap", GciJu4InheritedNativeProbe::hashMap, reps);
        report("linkedhashmap", GciJu4InheritedNativeProbe::linkedHashMap, reps);
        report("arraylist", GciJu4InheritedNativeProbe::arrayList, reps);
        report("arraydeque", GciJu4InheritedNativeProbe::arrayDeque, reps);
        report("properties", GciJu4InheritedNativeProbe::properties, reps);
        report("exception", GciJu4InheritedNativeProbe::exception, reps);
        report("classloader", GciJu4InheritedNativeProbe::classLoader, reps);
        report("thread", GciJu4InheritedNativeProbe::thread, threadReps);
        report("executor", GciJu4InheritedNativeProbe::executor, threadReps);
        System.out.println("ju4-inherited: done reps=" + reps);
    }
}
