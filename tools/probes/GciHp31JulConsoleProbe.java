// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.logging.LogManager;
import java.util.logging.Logger;

/**
 * gci i31/hp -- a JUL logger nobody configured
 * ({@code gci-i30gv-compatible-jul-console-sink-is-not-hotspots-console-handler-20261015}).
 * HotSpot 25: {@code Logger.getLogger} initializes the LogManager, whose
 * primordial configuration puts a {@code ConsoleHandler} on the root, so
 * {@code info} / {@code warning} print two {@code SimpleFormatter} lines each
 * on STDERR ({@code <date> <class> <method>} and {@code LEVEL: msg}),
 * {@code fine} prints nothing (root level INFO), and stdout carries only the
 * probe's own lines. Under {@code --compatible} CratonVM printed
 * {@code INFO [gcihp31.p] HP31-INFO} on STDOUT (the console sink).
 *
 * <pre>
 *   java     -Xmx64m GciHp31JulConsoleProbe plain|reset
 *   cratonvm --compatible -Xmx64m GciHp31JulConsoleProbe plain|reset
 * </pre>
 *
 * {@code reset}: {@code LogManager.getLogManager().reset()} first -- no
 * handler anywhere, HotSpot prints nothing at all for the records.
 *
 * Expected stdout (HotSpot 25, both modes): {@code hp31 mode=<mode> start},
 * {@code hp31 mode=<mode> done}. stderr (plain): {@code INFO: HP31-INFO},
 * {@code WARNING: HP31-WARN}, each after a date / source line naming
 * {@code GciHp31JulConsoleProbe main}; no {@code HP31-FINE}.
 */
public class GciHp31JulConsoleProbe {
    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "plain";
        System.out.println("hp31 mode=" + mode + " start");
        if (mode.equals("reset")) {
            LogManager.getLogManager().reset();
        }
        Logger p = Logger.getLogger("gcihp31.p");
        p.info("HP31-INFO");
        p.fine("HP31-FINE");
        p.warning("HP31-WARN");
        System.err.flush();
        System.out.println("hp31 mode=" + mode + " done");
    }
}
