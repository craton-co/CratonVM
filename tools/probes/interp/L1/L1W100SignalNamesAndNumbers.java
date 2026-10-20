// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L1 --
// docs/internal/fixed-bugs/interpreter-i100-L1-windows-signal-names-and-numbers-are-linuxs-FIXED-20261015.md
//
// `jdk.internal.misc.Signal` (through the exported `sun.misc.Signal`): for
// every name a JDK platform knows, whether `new Signal(name)` resolves, its
// number, and what `Signal.handle(sig, handler)` answers (the previous
// handler's kind, or the IllegalArgumentException's message). The handler is
// put back afterwards, so nothing is left installed. No signal is raised.
//
// The answers are platform-dependent; compare with HotSpot 25 on the same
// platform. Before wave 100 CratonVM answered the Linux table everywhere: on
// Windows `HUP`, `QUIT`, `USR1`, ... resolved (HotSpot: "Unknown signal"),
// `ABRT` was 6 (HotSpot: 22), and `BREAK` could be handled (HotSpot: "Signal
// already used by VM or OS: SIGBREAK").
//
//   javac -d out L1W100SignalNamesAndNumbers.java     (warns: sun.misc.Signal)
//   cratonvm -cp out L1W100SignalNamesAndNumbers
//   cratonvm --compatible -cp out L1W100SignalNamesAndNumbers
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, Windows; from jvm_windows.cpp /
// os_windows.cpp -- confirm on the first HotSpot run and refresh)
// HUP unknown: Unknown signal: HUP
// INT 2 handle=sun.misc.Signal$SunMiscHandler
// QUIT unknown: Unknown signal: QUIT
// ILL 4 handle=SIG_DFL
// ABRT 22 handle=SIG_DFL
// FPE 8 refused: Signal already used by VM or OS: SIGFPE
// KILL unknown: Unknown signal: KILL
// USR1 unknown: Unknown signal: USR1
// SEGV 11 handle=SIG_DFL
// USR2 unknown: Unknown signal: USR2
// PIPE unknown: Unknown signal: PIPE
// TERM 15 handle=sun.misc.Signal$SunMiscHandler
// BREAK 21 refused: Signal already used by VM or OS: SIGBREAK
// HOTSPOT_EXPECTED_END
// (Linux HotSpot: HUP 1, INT 2, TERM 15 handle=sun.misc.Signal$SunMiscHandler; QUIT/ILL/FPE/SEGV
// refused; KILL 9 refused by the OS; USR1 10 SIG_DFL; USR2 12 / PIPE 13 a
// native handler; BREAK unknown.)

import sun.misc.Signal;
import sun.misc.SignalHandler;

public class L1W100SignalNamesAndNumbers {
    public static void main(String[] args) {
        String[] names = {"HUP", "INT", "QUIT", "ILL", "ABRT", "FPE", "KILL", "USR1",
                          "SEGV", "USR2", "PIPE", "TERM", "BREAK"};
        SignalHandler mine = s -> { };
        for (String name : names) {
            Signal sig;
            try {
                sig = new Signal(name);
            } catch (IllegalArgumentException e) {
                System.out.println(name + " unknown: " + e.getMessage());
                continue;
            }
            String row = name + " " + sig.getNumber();
            try {
                SignalHandler old = Signal.handle(sig, mine);
                row += " handle=" + kind(old);
                // Put the previous disposition back.
                Signal.handle(sig, old);
            } catch (IllegalArgumentException e) {
                row += " refused: " + e.getMessage();
            }
            System.out.println(row);
        }
    }

    static String kind(SignalHandler h) {
        if (h == SignalHandler.SIG_DFL) {
            return "SIG_DFL";
        }
        if (h == SignalHandler.SIG_IGN) {
            return "SIG_IGN";
        }
        if (h == null) {
            return "null";
        }
        // A JDK-side handler (Terminator's, or the VM's native one) comes back
        // wrapped: `sun.misc.Signal$SunMiscHandler`.
        return h.getClass().getName();
    }
}
