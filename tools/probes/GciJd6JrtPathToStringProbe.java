// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Method;
import java.net.URI;
import java.nio.file.DirectoryStream;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;

/**
 * gci i6/jd: {@code toString()} of a real {@code jdk.internal.jrtfs.JrtPath}
 * through every dispatch shape javac's file manager uses: an interface call,
 * {@code String.valueOf(Object)}, string concatenation and reflection, on the
 * main thread and then from several threads at once on fresh paths.
 *
 * <p>Page: {@code docs/internal/gc/gci-i4orch-compatible-concurrent-javac-reads-a-module-info-class-as-source-FIXED-20261017.md}.
 * javac's {@code BaseFileManager.getKind(Path)} is
 * {@code path.getFileName().toString().endsWith(".class")}; under
 * {@code --compatible} a door that sent a {@code JrtPath} to this VM's carrier
 * {@code Path.toString} native answered something without that suffix.
 *
 * <p>stdout is deterministic (HotSpot 25 is the oracle); nothing on stderr.
 * Run: {@code cratonvm --compatible -cp <classes> GciJd6JrtPathToStringProbe 4}.
 */
public class GciJd6JrtPathToStringProbe {
    static String viaObject(Object o) {
        return String.valueOf(o);
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        Path mi = jrt.getPath("/modules/java.base/module-info.class");
        Path name = mi.getFileName();
        Method ts = Path.class.getMethod("toString");
        Method gfn = Path.class.getMethod("getFileName");
        System.out.println("class=" + name.getClass().getName());
        System.out.println("iface=" + name.toString());
        System.out.println("valueOf=" + viaObject(name));
        System.out.println("concat=" + ("" + name));
        System.out.println("reflect=" + ts.invoke(name));
        System.out.println("reflect-getFileName=" + ((Path) gfn.invoke(mi)).toString());
        System.out.println("full-iface=" + mi.toString());
        System.out.println("full-reflect=" + ts.invoke(mi));
        System.out.println("endsWith-class=" + name.toString().endsWith(".class"));

        ExecutorService ex = Executors.newFixedThreadPool(threads);
        CountDownLatch go = new CountDownLatch(1);
        List<Future<Integer>> results = new ArrayList<>();
        for (int t = 0; t < threads; t++) {
            Callable<Integer> task = () -> {
                go.await();
                int wrong = 0;
                int seen = 0;
                try (DirectoryStream<Path> ds = Files.newDirectoryStream(jrt.getPath("/modules"))) {
                    for (Path m : ds) {
                        Path f = m.resolve("module-info.class").getFileName();
                        if (!f.toString().endsWith(".class")) {
                            wrong++;
                        }
                        if (!viaObject(f).endsWith(".class")) {
                            wrong++;
                        }
                        if (!((String) ts.invoke(f)).endsWith(".class")) {
                            wrong++;
                        }
                        seen++;
                    }
                }
                return seen > 0 ? wrong : -1;
            };
            results.add(ex.submit(task));
        }
        go.countDown();
        int total = 0;
        for (Future<Integer> r : results) {
            total += r.get();
        }
        ex.shutdown();
        System.out.println("concurrent threads=" + threads + " wrong=" + total);
    }
}
