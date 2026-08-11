// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import javax.management.openmbean.CompositeData;
import javax.management.openmbean.CompositeDataSupport;
import javax.management.openmbean.CompositeType;
import javax.management.openmbean.OpenType;
import javax.management.openmbean.SimpleType;
import javax.management.openmbean.TabularDataSupport;
import javax.management.openmbean.TabularType;

/**
 * Reproduces the shape behind
 * {@code TestJMXAccessorTask.testCreatePropertyForTabularDataSupport}: a
 * real-JDK-constructed {@link CompositeDataSupport} nested as an item value of
 * an outer composite, plus a {@link TabularDataSupport} row.
 *
 * <p>Every line prints a self-describing PASS/FAIL so one run tells you which
 * of the carrier accessors ({@code getCompositeType}, {@code get},
 * {@code containsKey}, {@code getAll}, {@code TabularDataSupport.put/size/
 * isEmpty}) reads the real instance state and which reads a CratonVM synthetic
 * side field that a real constructor never wrote.
 */
public final class OpenMBeanCarrierProbe {

    private static int failures = 0;

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + " -> " + detail);
        if (!ok) {
            failures++;
        }
    }

    public static void main(String[] args) throws Exception {
        CompositeType inner = new CompositeType("details", "details", new String[] { "name" },
                new String[] { "name" }, new OpenType[] { SimpleType.STRING });

        CompositeData innerValue =
                new CompositeDataSupport(inner, new String[] { "name" }, new Object[] { "alpha" });

        CompositeType readBack = innerValue.getCompositeType();
        check("CompositeDataSupport.getCompositeType() is non-null", readBack != null,
                String.valueOf(readBack));
        check("CompositeDataSupport.getCompositeType() equals the declared type",
                inner.equals(readBack), String.valueOf(readBack));
        check("CompositeDataSupport.get(\"name\")", "alpha".equals(innerValue.get("name")),
                String.valueOf(innerValue.get("name")));
        check("CompositeDataSupport.containsKey(\"name\")", innerValue.containsKey("name"),
                String.valueOf(innerValue.containsKey("name")));
        Object[] all = innerValue.getAll(new String[] { "name" });
        check("CompositeDataSupport.getAll([name])",
                all != null && all.length == 1 && "alpha".equals(all[0]),
                all == null ? "null" : java.util.Arrays.toString(all));
        check("CompositeType.isValue(innerValue)", inner.isValue(innerValue),
                String.valueOf(inner.isValue(innerValue)));

        // The outer composite is where the suite failure surfaced: the
        // constructor validates each item value against its declared open type,
        // and that check goes through innerValue.getCompositeType().
        CompositeType outer = new CompositeType("row", "row", new String[] { "details" },
                new String[] { "details" }, new OpenType[] { inner });
        CompositeData row;
        try {
            row = new CompositeDataSupport(outer, new String[] { "details" },
                    new Object[] { innerValue });
            check("new CompositeDataSupport(outer, {details: innerValue})", true, row.toString());
        } catch (Exception e) {
            check("new CompositeDataSupport(outer, {details: innerValue})", false,
                    e.getClass().getName() + ": " + e.getMessage());
            row = null;
        }

        if (row != null) {
            TabularType tabularType =
                    new TabularType("rows", "rows", outer, new String[] { "details" });
            TabularDataSupport table = new TabularDataSupport(tabularType);
            check("TabularDataSupport.isEmpty() before put", table.isEmpty(),
                    String.valueOf(table.isEmpty()));
            table.put(row);
            check("TabularDataSupport.size() after one put", table.size() == 1,
                    String.valueOf(table.size()));
            check("TabularDataSupport.isEmpty() after one put", !table.isEmpty(),
                    String.valueOf(table.isEmpty()));
            Object viaGet = table.get(new Object[] { innerValue });
            check("TabularDataSupport.get(indexKey) returns the row", row.equals(viaGet),
                    String.valueOf(viaGet));
        }

        System.out.println(failures == 0 ? "PROBE OK" : "PROBE FAILURES=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }
}
