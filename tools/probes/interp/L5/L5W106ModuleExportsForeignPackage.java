// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 106, lane L5 -- `Module.isExported` / `isOpen`
// for a package the (named) module does not contain. JDK 25
// `Module.implIsExportedOrOpen` answers `false` for it in all four forms: the
// "exported/open to self" rule and the open/automatic-module rule both test
// `descriptor.packages().contains(pn)`. CratonVM's registry answered `true`
// for `m.isExported(p, m)` / `m.isOpen(p, m)` with ANY `p`
// (`classloading/src/module.rs` `is_package_exported_to` /
// `is_package_open_to`: `module_name == to_module`), and for any package of
// an open or automatic module. The natives behind these methods are forced
// over the JDK bytecode (`native_override.rs`), so `--compatible` at least
// runs them. Fix: `native-builtins/src/lib.rs` `named_module_lacks_package`.
//
// Run (no setup):
//   javac -d out L5W106ModuleExportsForeignPackage.java
//   java -cp out L5W106ModuleExportsForeignPackage                 (HotSpot 25)
//   cratonvm --jdk-only   [--nojit] -cp out L5W106ModuleExportsForeignPackage
//   cratonvm --compatible [--nojit] -cp out L5W106ModuleExportsForeignPackage
//
// HOTSPOT_EXPECTED derived from the JDK 25 sources, not run.
// HOTSPOT_EXPECTED_BEGIN
// base-exported-to-self-foreign=false
// base-open-to-self-foreign=false
// base-exported-to-self-other-modules-pkg=false
// base-open-to-self-other-modules-pkg=false
// base-exported-to-self-own-internal=true
// base-open-to-self-own-internal=true
// base-exported-foreign=false
// base-open-foreign=false
// base-exported-java-lang=true
// base-open-java-lang=false
// unnamed-exported-foreign=true
// unnamed-open-foreign-to-base=true
// HOTSPOT_EXPECTED_END

public class L5W106ModuleExportsForeignPackage {
    public static void main(String[] args) {
        Module base = Object.class.getModule();
        Module unnamed = L5W106ModuleExportsForeignPackage.class.getModule();
        System.out.println("base-exported-to-self-foreign=" + base.isExported("no.such.pkg", base));
        System.out.println("base-open-to-self-foreign=" + base.isOpen("no.such.pkg", base));
        System.out.println("base-exported-to-self-other-modules-pkg=" + base.isExported("java.sql", base));
        System.out.println("base-open-to-self-other-modules-pkg=" + base.isOpen("java.sql", base));
        System.out.println("base-exported-to-self-own-internal=" + base.isExported("jdk.internal.misc", base));
        System.out.println("base-open-to-self-own-internal=" + base.isOpen("jdk.internal.misc", base));
        System.out.println("base-exported-foreign=" + base.isExported("no.such.pkg"));
        System.out.println("base-open-foreign=" + base.isOpen("no.such.pkg"));
        System.out.println("base-exported-java-lang=" + base.isExported("java.lang"));
        System.out.println("base-open-java-lang=" + base.isOpen("java.lang"));
        System.out.println("unnamed-exported-foreign=" + unnamed.isExported("no.such.pkg"));
        System.out.println("unnamed-open-foreign-to-base=" + unnamed.isOpen("no.such.pkg", base));
    }
}
