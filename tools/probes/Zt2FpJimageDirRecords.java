// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t2/fp (2026-10-19): does a runtime-image DIRECTORY record answer as a
// resource or a module? The VM's jimage index files every entry that is not a
// `.class` under resource_to_modules, including the ~2 300 records of the
// image's /packages/<pkg>/<module> and /modules trees, under the
// pseudo-modules `packages` and `modules` (classloading/src/class_path.rs,
// ClassPathEntry::index_jimage_entry). HotSpot serves neither as a resource
// and has no such modules. Page:
// docs/internal/gc/zgc-zt2fp-jimage-directory-records-are-indexed-as-resources-CLOSED-20261019.md.
//
// Every stdout line is deterministic and must equal HotSpot's byte for byte.
// The first name is the positive control (a real resource, `.class`, so not
// encapsulated: found on every loader). The second is a real java.base
// resource in a package java.base does not open, so HotSpot's answer for it is
// the encapsulation rule's (null through these doors), whatever this VM's
// index holds.
//
// Usage: Zt2FpJimageDirRecords
import java.io.InputStream;
import java.net.URL;
import java.util.Enumeration;

public class Zt2FpJimageDirRecords {
    static final String[] NAMES = {
        "java/lang/Object.class",
        "java/lang/uniName.dat",
        "java.lang/java.base",
        "java.util/java.base",
        "java.lang",
        "java.base",
        "java.base/java/lang/Object.class",
        "packages/java.lang/java.base",
        "modules/java.base/java/lang/Object.class",
    };

    static String present(URL u) {
        return u == null ? "null" : "found";
    }

    static int count(Enumeration<URL> e) {
        int n = 0;
        while (e.hasMoreElements()) {
            e.nextElement();
            n++;
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        for (String name : NAMES) {
            String viaClass;
            try (InputStream in = Object.class.getResourceAsStream("/" + name)) {
                viaClass = in == null ? "null" : "found";
            }
            System.out.println("res " + name
                    + " app=" + present(app.getResource(name))
                    + " platform=" + present(platform.getResource(name))
                    + " app_all=" + count(app.getResources(name))
                    + " object_class=" + viaClass);
        }
        ModuleLayer boot = ModuleLayer.boot();
        for (String m : new String[] {"java.base", "packages", "modules"}) {
            System.out.println("module " + m + " present=" + boot.findModule(m).isPresent());
        }
        System.out.println("zt2fp jimage-dir-records done");
    }
}
