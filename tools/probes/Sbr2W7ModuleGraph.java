// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Optional;
import java.util.Set;
import java.util.TreeSet;

/**
 * SBR2 wave 7, lane MODULE: the whole boot module graph, asked of the JDK's own
 * {@code Module}: for every pair of boot modules {@code canRead}, for every package of every module
 * {@code isExported}/{@code isOpen} to nobody, to itself, and to each other module, and the layer
 * accessors. One line per module, so a diff against HotSpot names the module. Both ends of each
 * predicate occur (java.base exports java.lang to all and jdk.internal.misc only to a few).
 *
 * <pre>
 *   java -cp . Sbr2W7ModuleGraph                    (HotSpot oracle)
 *   cratonvm --jdk-only -cp . Sbr2W7ModuleGraph
 * </pre>
 */
public class Sbr2W7ModuleGraph {
    static String tf(boolean b) {
        return b ? "1" : "0";
    }

    public static void main(String[] args) throws Throwable {
        ModuleLayer boot = ModuleLayer.boot();
        List<Module> mods = new ArrayList<>(boot.modules());
        // Only the modules every JDK image of this build has (the Windows Temurin the oracle runs
        // on and the linux image the VM boots from differ in the jdk.* tail), so a diff is about
        // the module graph and not about which image was used.
        final Set<String> common = Set.of(
            "java.base", "java.compiler", "java.datatransfer", "java.desktop", "java.instrument",
            "java.logging", "java.management", "java.management.rmi", "java.naming", "java.net.http",
            "java.prefs", "java.rmi", "java.scripting", "java.security.jgss", "java.security.sasl",
            "java.smartcardio", "java.sql", "java.sql.rowset", "java.transaction.xa", "java.xml",
            "java.xml.crypto", "jdk.accessibility", "jdk.attach", "jdk.charsets", "jdk.compiler",
            "jdk.crypto.cryptoki", "jdk.dynalink", "jdk.editpad", "jdk.httpserver", "jdk.internal.ed",
            "jdk.internal.jvmstat", "jdk.internal.le", "jdk.internal.md", "jdk.internal.opt",
            "jdk.jartool", "jdk.javadoc", "jdk.jconsole", "jdk.jdeps", "jdk.jdi", "jdk.jdwp.agent",
            "jdk.jfr", "jdk.jlink", "jdk.jpackage", "jdk.jshell", "jdk.jsobject", "jdk.jstatd",
            "jdk.localedata", "jdk.management", "jdk.management.agent", "jdk.management.jfr",
            "jdk.naming.dns", "jdk.naming.rmi", "jdk.net", "jdk.nio.mapmode", "jdk.sctp",
            "jdk.security.auth", "jdk.security.jgss", "jdk.unsupported", "jdk.unsupported.desktop",
            "jdk.xml.dom", "jdk.zipfs");
        mods.removeIf(m -> !common.contains(m.getName()));
        mods.sort(Comparator.comparing(Module::getName));
        System.out.println("layer.modules.size>=60 " + (mods.size() >= 60) + " " + mods.size());
        System.out.println("boot.parents " + boot.parents().size());
        Module self = Sbr2W7ModuleGraph.class.getModule();
        Module base = Object.class.getModule();
        for (Module m : mods) {
            StringBuilder reads = new StringBuilder();
            for (Module o : mods) {
                reads.append(tf(m.canRead(o)));
            }
            System.out.println("reads " + m.getName() + " " + reads);
            System.out.println("readsUnnamed " + m.getName() + " " + tf(m.canRead(self)));
        }
        for (Module m : mods) {
            Set<String> pkgs = new TreeSet<>(m.getPackages());
            List<String> rows = new ArrayList<>();
            for (String p : pkgs) {
                StringBuilder row = new StringBuilder();
                row.append(tf(m.isExported(p))).append(tf(m.isOpen(p)));
                row.append(tf(m.isExported(p, self))).append(tf(m.isOpen(p, self)));
                row.append(tf(m.isExported(p, base))).append(tf(m.isOpen(p, base)));
                row.append(tf(m.isExported(p, m))).append(tf(m.isOpen(p, m)));
                for (Module o : mods) {
                    row.append(tf(m.isExported(p, o)));
                }
                rows.add(row.toString());
            }
            System.out.println("pkgs " + m.getName() + " n=" + rows.size() + " h=" + String.join("|", rows).hashCode());
            String n = m.getName();
            if (n.equals("java.base") || n.equals("java.logging") || n.equals("java.sql")) {
                int i = 0;
                for (String p : pkgs) {
                    System.out.println("  " + p + " " + rows.get(i++).substring(0, 8));
                }
            }
        }
        for (String n : new String[] {"java.base", "java.sql", "jdk.zipfs", "no.such.module", "java.logging"}) {
            Optional<Module> o = boot.findModule(n);
            System.out.println("findModule " + n + " " + o.isPresent()
                    + (o.isPresent() ? " " + (o.get() == ModuleLayer.boot().findModule(n).get()) : ""));
        }
        System.out.println("base==Object.getModule " + (boot.findModule("java.base").get() == base));
        System.out.println("base.layer==boot " + (base.getLayer() == boot));
        System.out.println("modules.contains(base) " + boot.modules().contains(base));
        System.out.println("modules.same " + (boot.modules() == boot.modules()));
        System.out.println("base.canRead(unnamed) " + tf(base.canRead(self)));
        System.out.println("unnamed.canRead(base) " + tf(self.canRead(base)));
        Module sql = boot.findModule("java.sql").get();
        System.out.println("sql.isExported(java.sql) " + tf(sql.isExported("java.sql")));
        System.out.println("sql.isOpen(java.sql) " + tf(sql.isOpen("java.sql")));
        System.out.println("sql.isExported(nosuch) " + tf(sql.isExported("no.such.pkg")));
        try {
            System.out.println("before " + base.isExported("jdk.internal.misc", self));
            base.addExports("jdk.internal.misc", self);
            System.out.println("after " + base.isExported("jdk.internal.misc", self));
        } catch (Throwable t) {
            System.out.println("addExports " + t.getClass().getName() + ": "
                    + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H"));
        }
        try {
            sql.isExported(null);
        } catch (Throwable t) {
            System.out.println("isExported(null) " + t.getClass().getName());
        }
        try {
            sql.canRead(null);
        } catch (Throwable t) {
            System.out.println("canRead(null) " + t.getClass().getName());
        }
        System.out.println("descriptor " + finderDescriptor());
        // the public mutators, from an UNNAMED caller: named receivers must refuse (caller != module),
        // the unnamed receiver is a documented no-op that answers itself
        Module logging = boot.findModule("java.logging").get();
        for (String op : new String[] {"addReads", "addExports", "addOpens", "addUses"}) {
            for (Module recv : new Module[] {base, logging, self}) {
                String what = recv == base ? "base" : recv == logging ? "logging" : "unnamed";
                try {
                    Object r;
                    if (op.equals("addReads")) {
                        r = recv.addReads(self) == recv;
                    } else if (op.equals("addExports")) {
                        r = recv.addExports("java.util.logging", self) == recv;
                    } else if (op.equals("addOpens")) {
                        r = recv.addOpens("java.util.logging", self) == recv;
                    } else {
                        r = recv.addUses(Runnable.class) == recv;
                    }
                    System.out.println(op + " " + what + " " + r);
                } catch (Throwable t) {
                    System.out.println(op + " " + what + " " + t.getClass().getName() + ": "
                            + String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H"));
                }
            }
        }
        System.out.println("logging.isExported(util.logging,self) " + tf(logging.isExported("java.util.logging", self)));
        System.out.println("logging.isOpen(util.logging,self) " + tf(logging.isOpen("java.util.logging", self)));
        System.out.println("logging.isOpen(util.logging) " + tf(logging.isOpen("java.util.logging")));
        System.out.println("logging.isExported(nosuch,self) " + tf(logging.isExported("no.such", self)));
        System.out.println("base.isOpen(java.lang) " + tf(base.isOpen("java.lang")));
        System.out.println("canUse " + tf(base.canUse(Runnable.class)) + tf(self.canUse(Runnable.class)));
    }

    static String finderDescriptor() {
        try {
            java.lang.module.ModuleFinder f = java.lang.module.ModuleFinder.ofSystem();
            java.lang.module.ModuleReference r = f.find("java.sql").get();
            return r.descriptor().name() + " " + (r.descriptor().exports().size() > 0)
                    + " " + f.find("no.such.mod").isPresent();
        } catch (Throwable t) {
            return "!" + t;
        }
    }
}
