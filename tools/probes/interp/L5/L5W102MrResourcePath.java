// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 102, lane L5
// (docs/internal/fixed-bugs/interpreter-i102-L5-jdk21-jdk-only-class-path-resource-null-FIXED-20261015.md).
//
// Splits the class-path resource path of a Multi-Release jar into its steps,
// so ONE run under `--jdk-only` over a JDK 21 library shows which step answers
// differently from HotSpot 21. Uses the jar L5W100MultiReleaseFeature/setup.sh
// builds. From tools/probes/interp/L5/L5W100MultiReleaseFeature:
//
//   sh setup.sh
//   javac --release 10 -d out/L5W102 ../L5W102MrResourcePath.java
//   <java>   -cp out/L5W100MR/mr.jar:out/L5W102 L5W102MrResourcePath out/L5W100MR/mr.jar
//   cratonvm --java-home <jdk> [--nojit] [--compatible] -cp out/L5W100MR/mr.jar:out/L5W102 L5W102MrResourcePath out/L5W100MR/mr.jar
//
// Expected, F = the booted library's feature version (21 or 25), HotSpot:
//   feature=F
//   first-app-as-stream=F
//   app-as-stream=F
//   app-url=jar:.../mr.jar!/META-INF/versions/F/mr/v.txt
//   app-url-stream=F
//   sys-url=jar:.../mr.jar!/META-INF/versions/F/mr/v.txt
//   sys-stream=F
//   sys-as-stream=F
//   sys-class-url=jar:.../mr.jar!/META-INF/versions/F/mr/V.class
//   jar-multi=true
//   jar-entry=mr/v.txt real=META-INF/versions/F/mr/v.txt
//   jar-read=F
//   jar-base-entry=META-INF/versions/9/mr/v.txt real=META-INF/versions/9/mr/v.txt
//   ucl-url=jar:.../mr.jar!/META-INF/versions/F/mr/v.txt
//   ucl-stream=F
// (`jar:...` prints only the part from `!/`, so the lines are machine-free.)
// A step that throws prints `<row>=EXC <class>: <message>`.

import java.io.File;
import java.io.InputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.jar.JarEntry;
import java.util.jar.JarFile;
import java.util.zip.ZipFile;

public class L5W102MrResourcePath {
    interface Step {
        String run() throws Exception;
    }

    static void row(String name, Step step) {
        String out;
        try {
            out = step.run();
        } catch (Throwable t) {
            out = "EXC " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + "=" + out);
    }

    static String tail(URL url) {
        if (url == null) {
            return "null";
        }
        String s = url.toString();
        int bang = s.indexOf("!/");
        return bang < 0 ? s : "jar:.../mr.jar" + s.substring(bang);
    }

    static String read(InputStream in) throws Exception {
        if (in == null) {
            return "null";
        }
        try (InputStream i = in) {
            return new String(i.readAllBytes(), "UTF-8");
        }
    }

    public static void main(String[] args) throws Exception {
        // FIRST action of the run, before any other resource / jar / version
        // API (orchestrator, wave 102: L5W100MultiReleaseFeature, which does
        // exactly this, printed resource=null under --jdk-only over JDK 21
        // while every later row of this probe matched HotSpot). Printed after
        // `feature=` so the line order stays the header's.
        String first;
        try {
            first = read(L5W102MrResourcePath.class.getClassLoader().getResourceAsStream("mr/v.txt"));
        } catch (Throwable t) {
            first = "EXC " + t.getClass().getName() + ": " + t.getMessage();
        }
        String jar = args.length > 0 ? args[0] : "out/L5W100MR/mr.jar";
        System.out.println("feature=" + Runtime.version().feature());
        System.out.println("first-app-as-stream=" + first);
        ClassLoader app = L5W102MrResourcePath.class.getClassLoader();
        row("app-as-stream", () -> read(app.getResourceAsStream("mr/v.txt")));
        row("app-url", () -> tail(app.getResource("mr/v.txt")));
        row("app-url-stream", () -> {
            URL u = app.getResource("mr/v.txt");
            return u == null ? "null" : read(u.openStream());
        });
        row("sys-url", () -> tail(ClassLoader.getSystemResource("mr/v.txt")));
        row("sys-stream", () -> {
            URL u = ClassLoader.getSystemResource("mr/v.txt");
            return u == null ? "null" : read(u.openStream());
        });
        row("sys-as-stream", () -> read(ClassLoader.getSystemResourceAsStream("mr/v.txt")));
        row("sys-class-url", () -> tail(ClassLoader.getSystemResource("mr/V.class")));
        try (JarFile jf = new JarFile(new File(jar), true, ZipFile.OPEN_READ, Runtime.version())) {
            row("jar-multi", () -> String.valueOf(jf.isMultiRelease()));
            row("jar-entry", () -> {
                JarEntry e = jf.getJarEntry("mr/v.txt");
                return e == null ? "null" : e.getName() + " real=" + e.getRealName();
            });
            row("jar-read", () -> {
                JarEntry e = jf.getJarEntry("mr/v.txt");
                return e == null ? "null" : read(jf.getInputStream(e));
            });
            row("jar-base-entry", () -> {
                JarEntry e = jf.getJarEntry("META-INF/versions/9/mr/v.txt");
                return e == null ? "null" : e.getName() + " real=" + e.getRealName();
            });
        }
        URL jarUrl = new File(jar).toURI().toURL();
        try (URLClassLoader ucl = new URLClassLoader(new URL[] {jarUrl}, null)) {
            row("ucl-url", () -> tail(ucl.getResource("mr/v.txt")));
            row("ucl-stream", () -> read(ucl.getResourceAsStream("mr/v.txt")));
        }
    }
}
