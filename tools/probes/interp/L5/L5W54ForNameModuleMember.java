// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 54, lane L5 -- `Class.forName(Module, String)`
// answers a class only when it is a MEMBER of that module (the JDK's
// `ClassLoader.loadClass(Module, String)`: `c.getModule() == module`, else
// null). CratonVM's native for it (`native-builtins/src/lang_class.rs`
// `native_class_for_name_module`, `--compatible`; `--jdk-only` runs the JDK
// body) looked the name up in the flat store and answered any class of that
// name: `Class.forName(<java.sql>, "java.lang.String")` gave `String`.
//
// Rows (first on a cold VM, again last): a named module and its own class, the
// same module and another module's class, `java.base` both ways, the
// application loader's unnamed module and this class / a `java.base` class.
//
// Run (no setup):
//   javac -d out L5W54ForNameModuleMember.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W54ForNameModuleMember
//
// Measured on a local dev build before the fix (wave-53 head plus this
// lane's other changes, Windows): `--compatible` printed
// `sql-string-N=class java.lang.String`, `base-date-N=class java.sql.Date` and
// `unnamed-string-N=class java.lang.String`; `--jdk-only` matched HotSpot.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   sql-string-1=null
//   sql-date-1=class java.sql.Date
//   base-string-1=class java.lang.String
//   base-date-1=null
//   unnamed-self-1=class L5W54ForNameModuleMember
//   unnamed-string-1=null
//   sql-string-2=null
//   sql-date-2=class java.sql.Date
//   base-string-2=class java.lang.String
//   base-date-2=null
//   unnamed-self-2=class L5W54ForNameModuleMember
//   unnamed-string-2=null

public class L5W54ForNameModuleMember {
    static void row(String label, Module module, String name) {
        try {
            System.out.println(label + "=" + Class.forName(module, name));
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    static void round(int n) {
        Module sql = ModuleLayer.boot().findModule("java.sql").orElseThrow();
        Module base = Object.class.getModule();
        Module unnamed = ClassLoader.getSystemClassLoader().getUnnamedModule();
        row("sql-string-" + n, sql, "java.lang.String");
        row("sql-date-" + n, sql, "java.sql.Date");
        row("base-string-" + n, base, "java.lang.String");
        row("base-date-" + n, base, "java.sql.Date");
        row("unnamed-self-" + n, unnamed, "L5W54ForNameModuleMember");
        row("unnamed-string-" + n, unnamed, "java.lang.String");
    }

    public static void main(String[] args) {
        round(1);
        round(2);
    }
}
