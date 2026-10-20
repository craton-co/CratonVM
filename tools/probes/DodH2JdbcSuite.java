// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Definition-of-done driver: H2's OWN JDBC test classes, in one process,
// returning normally.
//
// Every concrete org.h2.test class carries
//   public static void main(String... a) { TestBase.createCaller().init().testFromMain(); }
// and `testFromMain` lets a failure propagate out of main -- so an exception
// out of the reflective call is the failure signal, and there is nothing to
// parse. None of them calls System.exit, which is what makes the --jdk-only
// report reachable at all: it is not written on the System.exit path.
//
// Prints only the class name and whether it threw. Never a timing, never a
// count the engine chooses.

import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.List;

public final class DodH2JdbcSuite {

    public static void main(String[] args) {
        List<String> failed = new ArrayList<>();
        int ran = 0;
        for (String name : args) {
            System.out.println("DOD H2-BEGIN " + name);
            try {
                Class<?> c = Class.forName(name);
                Method m = c.getMethod("main", String[].class);
                m.invoke(null, (Object) new String[0]);
                ran++;
                System.out.println("DOD H2-OK " + name);
            } catch (InvocationTargetException e) {
                failed.add(name);
                Throwable cause = e.getCause() == null ? e : e.getCause();
                System.out.println("DOD H2-FAIL " + name + " " + cause.getClass().getName()
                        + ": " + cause.getMessage());
                cause.printStackTrace(System.out);
            } catch (Throwable t) {
                failed.add(name);
                System.out.println("DOD H2-BROKEN " + name + " " + t.getClass().getName()
                        + ": " + t.getMessage());
                t.printStackTrace(System.out);
            }
        }
        System.out.println("DOD TOTAL ok=" + ran + " failed=" + failed.size()
                + "/" + args.length);
        for (String f : failed) {
            System.out.println("DOD FAILED-CLASS " + f);
        }
        System.out.println("DOD RESULT " + (failed.isEmpty() ? "OK" : "FAILURES=" + failed.size()));
    }
}
