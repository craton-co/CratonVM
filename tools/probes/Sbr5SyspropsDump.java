// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Properties;
import java.util.TreeMap;

/**
 * SBR round 5, lane {@code sysprops}: the real {@code System.props} content, sorted, one
 * {@code key=value} per line, plus the keys where {@code System.getProperty} (the VM's native
 * reader) and the {@code Properties} object disagree. Diff against HotSpot; paths differ
 * legitimately ({@code java.home}, {@code java.class.path}, {@code sun.java.command}...).
 */
public final class Sbr5SyspropsDump {
    public static void main(String[] a) {
        Properties p = System.getProperties();
        TreeMap<String, String> m = new TreeMap<>();
        for (String k : p.stringPropertyNames()) m.put(k, p.getProperty(k));
        for (var e : m.entrySet()) {
            String viaSystem = System.getProperty(e.getKey());
            System.out.println(e.getKey() + "=" + e.getValue() + (e.getValue().equals(viaSystem) ? "" : "   <<< System.getProperty=" + viaSystem));
        }
    }
}
