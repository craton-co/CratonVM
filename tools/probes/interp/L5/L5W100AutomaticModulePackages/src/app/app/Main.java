// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 100, lane L5 -- an AUTOMATIC module's package
// set, name and version as the JDK's `ModulePath.deriveModuleDescriptor`
// derives them (`classloading/src/module.rs` `automatic_jar_module`). Its
// packages are those of its CLASS files only: `templates/` and `conf/x/`
// (resources only) and `META-INF/...` are no packages. The name comes from
// `Automatic-Module-Name`, the version from the file name's tail.
//
// Setup and run from L5W100AutomaticModulePackages/ (JDK 25 javac/jar on PATH):
//   sh setup.sh
//   java --module-path out/L5W100AM/mp -m app/app.Main
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] --module-path out/L5W100AM/mp -m app/app.Main
//
// Expected HotSpot 25 output:
//   call=lib.A
//   name=lib.auto automatic=true open=false
//   version=2.5.1-rc1
//   packages=[lib, lib.sub]
//   templates-resource=true
//   reads-app=true
//   app-in-module=app
//   other-resolved=true
//   app-reads-other=true

package app;

import java.io.InputStream;
import java.util.TreeSet;

public class Main {
    public static void main(String[] args) throws Exception {
        System.out.println("call=" + lib.A.hello());
        Module auto = lib.A.class.getModule();
        System.out.println("name=" + auto.getName() + " automatic="
                + auto.getDescriptor().isAutomatic() + " open=" + auto.getDescriptor().isOpen());
        System.out.println("version=" + auto.getDescriptor().rawVersion().orElse("none"));
        System.out.println("packages=" + new TreeSet<>(auto.getPackages()));
        // A resource outside every package is never encapsulated.
        try (InputStream in = auto.getResourceAsStream("templates/page.txt")) {
            System.out.println("templates-resource=" + (in != null));
        }
        System.out.println("reads-app=" + auto.canRead(Main.class.getModule()));
        System.out.println("app-in-module=" + Main.class.getModule().getName());
        // Resolving one automatic module resolves them all, and a module that
        // reads one automatic module reads every automatic module.
        java.util.Optional<Module> other = ModuleLayer.boot().findModule("other");
        System.out.println("other-resolved=" + other.isPresent());
        System.out.println("app-reads-other="
                + other.map(o -> Main.class.getModule().canRead(o)).orElse(false));
    }
}
