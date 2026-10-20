// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// User-loader java.* resolution: runtime references, initiating memo, two
// loaders, refusal, null and wrong-name answers. Compare stdout with HotSpot 25.
import java.nio.file.*;
import java.util.*;
public class I47JavaNameLoads {
    public interface Work { int hot(int n); int cold(); }
    @Deprecated(since="i47", forRemoval=true)
    public static class Holder implements Work {
        public int hot(int n) { return n + 1; }
        public int cold() {
            Object queue = new java.util.concurrent.LinkedTransferQueue<>();
            Object array = new java.util.concurrent.ConcurrentSkipListSet<?>[0];
            Object literal = java.util.concurrent.DelayQueue.class;
            Object field = java.util.Locale.ROOT;
            long clock = java.lang.System.nanoTime();
            return queue != null && array != null && literal != null && field != null && clock != 0 ? 1 : 0;
        }
    }
    public static class Other extends Holder {
        public int cold() { return new java.util.concurrent.LinkedTransferQueue<>().size() + 1; }
    }
    static final class Loader extends ClassLoader {
        final Path dir;
        final String refusal;
        final Map<String,Integer> calls = new TreeMap<>();
        boolean logging;
        Loader(Path dir, String refusal) { super(I47JavaNameLoads.class.getClassLoader()); this.dir=dir; this.refusal=refusal; }
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (logging && name.startsWith("java.")) calls.merge(name, 1, Integer::sum);
            if (name.equals("java.util.concurrent.LinkedTransferQueue")) {
                if (refusal.equals("throw")) throw new ClassNotFoundException("denied-java-name");
                if (refusal.equals("null")) return null;
                if (refusal.equals("wrong")) return java.util.List.class;
            }
            if (!name.equals("I47JavaNameLoads$Holder") && !name.equals("I47JavaNameLoads$Other")) {
                if (refusal.equals("race") && name.equals("java.util.concurrent.LinkedTransferQueue")) {
                    try { Thread.sleep(50); } catch (InterruptedException e) { throw new AssertionError(e); }
                }
                return getParent().loadClass(name);
            }
            Class<?> c=findLoadedClass(name);
            if (c == null) {
                try { byte[] b=Files.readAllBytes(dir.resolve(name+".class")); c=defineClass(name,b,0,b.length); }
                catch (java.io.IOException e) { throw new ClassNotFoundException(name,e); }
            }
            return c;
        }
    }
    public static void main(String[] args) throws Exception {
        Path dir=Path.of(args[0]);
        for (String refusal : new String[]{"delegate","delegate","throw","null","wrong"}) {
            Loader loader=new Loader(dir,refusal);
            Work work=(Work)loader.loadClass("I47JavaNameLoads$Holder").getConstructor().newInstance();
            loader.logging=true;
            for (int i=0;i<20000;i++) work.hot(i);
            for (int i=0;i<2;i++) {
                try { System.out.println(refusal+" result="+work.cold()); }
                catch (Throwable t) {
                    System.out.println(refusal+" error="+t.getClass().getName()+" ["+t.getMessage()+"]");
                }
            }
            System.out.println(refusal+" calls="+loader.calls);
            if (refusal.equals("delegate")) {
                loader.logging=false;
                Deprecated annotation=work.getClass().getAnnotation(Deprecated.class);
                System.out.println("annotation="+annotation.since()+"/"+annotation.forRemoval());
            }
        }
        Loader loader=new Loader(dir,"race");
        Work one=(Work)loader.loadClass("I47JavaNameLoads$Holder").getConstructor().newInstance();
        Work two=(Work)loader.loadClass("I47JavaNameLoads$Other").getConstructor().newInstance();
        loader.logging=true;
        java.util.concurrent.CountDownLatch start=new java.util.concurrent.CountDownLatch(1);
        java.util.concurrent.atomic.AtomicInteger successes=new java.util.concurrent.atomic.AtomicInteger();
        Thread a=new Thread(() -> { try { start.await(); if(one.cold()==1) successes.incrementAndGet(); } catch(Exception e) { throw new AssertionError(e); } });
        Thread b=new Thread(() -> { try { start.await(); if(two.cold()==1) successes.incrementAndGet(); } catch(Exception e) { throw new AssertionError(e); } });
        a.start(); b.start(); start.countDown(); a.join(); b.join();
        if (successes.get()!=2) throw new AssertionError("racing resolution failed");
        System.out.println("race queue-calls="+loader.calls.get("java.util.concurrent.LinkedTransferQueue"));
    }
}
