// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 115, lane L1 -- Windows `Ctrl-C` and console close
// reach the VM's ordinary exit path, as HotSpot 25's `consoleHandler`
// (`CRATONVM_WINDOWS_CONSOLE_SIGNALS`, default on, `=0` off). Page:
// docs/internal/fixed-bugs/interpreter-i115-L1-windows-ctrl-c-skips-shutdown-hooks-FIXED-20261019.md.
//
// Battery mode (no argument): the Java half of the path, which a real
// `Ctrl-C` ends in -- `sun.misc.Signal.raise(INT)` runs the JDK's
// `Terminator` handler, which runs the shutdown hooks and halts with 130.
// Compared lines: the hook's line; the exit status is 130 on HotSpot (the
// battery may not compare it).
//
// Manual mode (`wait`): run in a console window, press Ctrl-C within 60 s.
// HotSpot 25 prints "hook ran" and exits 130 (`echo %ERRORLEVEL%`). Before
// wave 115 CratonVM on Windows printed nothing and exited -1073741510
// (STATUS_CONTROL_C_EXIT). With `handler` the Java `Signal.handle("INT")`
// handler runs instead (prints "INT handled", keeps running; Ctrl-Break or
// closing the window ends it).
//
//   java L1W115ConsoleCtrlC wait           cratonvm L1W115ConsoleCtrlC wait
//   java L1W115ConsoleCtrlC handler        cratonvm L1W115ConsoleCtrlC handler
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, run; exit status 130)
// raising INT
// hook ran
// HOTSPOT_EXPECTED_END

import sun.misc.Signal;

public class L1W115ConsoleCtrlC {
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "raise";
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            System.out.println("hook ran");
            System.out.flush();
        }));
        switch (mode) {
            case "wait" -> {
                System.out.println("press Ctrl-C within 60 s");
                System.out.flush();
                Thread.sleep(60_000);
                System.out.println("no Ctrl-C");
            }
            case "handler" -> {
                Signal.handle(new Signal("INT"), s -> {
                    System.out.println("INT handled");
                    System.out.flush();
                });
                System.out.println("press Ctrl-C (handled), then close the window");
                System.out.flush();
                Thread.sleep(120_000);
            }
            default -> {
                System.out.println("raising INT");
                System.out.flush();
                Signal.raise(new Signal("INT"));
                Thread.sleep(10_000);
                System.out.println("not reached");
            }
        }
    }
}
