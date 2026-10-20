// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4
// (`i46-L4-platform-module-classes-are-named-loader-bootstrap-in-hotspot-worded-messages`):
// a failing `checkcast` whose operand is a class of a module the JDK's
// `ModuleLoaderMap` assigns to the PLATFORM loader (`java.sql`) names that
// loader `'platform'` in HotSpot's `ClassCastException` text. CratonVM
// defines every image class in its boot namespace and printed `'bootstrap'`.
// `java.desktop` is a BOOT module (`java.awt.Point` stays `'bootstrap'`).
//
// Positive control: row `sql-string` (and `L4W46LambdaSiteForeignStaticArg`
// row `sql-0`). The base printed `java.sql.Date is in module java.sql of
// loader 'bootstrap'`.
//
// Run: javac -d out L4W47PlatformLoaderCastText.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W47PlatformLoaderCastText
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   sql-string: class java.sql.Date cannot be cast to class java.lang.String (java.sql.Date is in module java.sql of loader 'platform'; java.lang.String is in module java.base of loader 'bootstrap')
//   string-sql: class java.lang.String cannot be cast to class java.sql.Time (java.lang.String is in module java.base of loader 'bootstrap'; java.sql.Time is in module java.sql of loader 'platform')
//   sql-sql: class java.sql.Date cannot be cast to class java.sql.Time (java.sql.Date and java.sql.Time are in module java.sql of loader 'platform')
//   sql-app: class java.sql.Date cannot be cast to class L4W47PlatformLoaderCastText (java.sql.Date is in module java.sql of loader 'platform'; L4W47PlatformLoaderCastText is in unnamed module of loader 'app')
//   sql-array: class [Ljava.sql.Date; cannot be cast to class [Ljava.lang.String; ([Ljava.sql.Date; is in module java.sql of loader 'platform'; [Ljava.lang.String; is in module java.base of loader 'bootstrap')
//   awt-string: class java.awt.Point cannot be cast to class java.lang.String (java.awt.Point is in module java.desktop of loader 'bootstrap'; java.lang.String is in module java.base of loader 'bootstrap')
//   loader: platform
public class L4W47PlatformLoaderCastText {
    static String cast(Object o, Class<?> to) {
        try {
            if (to == String.class) {
                String s = (String) o;
            } else if (to == java.sql.Time.class) {
                java.sql.Time t = (java.sql.Time) o;
            } else if (to == String[].class) {
                String[] a = (String[]) o;
            } else {
                L4W47PlatformLoaderCastText p = (L4W47PlatformLoaderCastText) o;
            }
            return "no exception";
        } catch (ClassCastException e) {
            return e.getMessage();
        }
    }

    public static void main(String[] args) {
        Object date = new java.sql.Date(0L);
        System.out.println("sql-string: " + cast(date, String.class));
        System.out.println("string-sql: " + cast("x", java.sql.Time.class));
        System.out.println("sql-sql: " + cast(date, java.sql.Time.class));
        System.out.println("sql-app: " + cast(date, L4W47PlatformLoaderCastText.class));
        System.out.println("sql-array: " + cast(new java.sql.Date[0], String[].class));
        System.out.println("awt-string: " + cast(new java.awt.Point(), String.class));
        System.out.println("loader: " + java.sql.Date.class.getClassLoader().getName());
    }
}
