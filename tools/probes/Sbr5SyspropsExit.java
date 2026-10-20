// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 5, lane {@code sysprops}: {@code System.exit} / {@code Runtime.exit} / {@code halt}
 * through the JDK's own {@code Shutdown} path. One mode per process; diff stdout AND the exit
 * code against HotSpot ({@code echo rc=$?}).
 *
 * <pre>cratonvm --jdk-only -cp . Sbr5SyspropsExit exit0|exit3|hooks|threadexit|haltnohook|exithook|runtimeexit|nested|flush|dtor</pre>
 */
public final class Sbr5SyspropsExit {
    static void hook(String n) {
        Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("hook " + n), "h-" + n));
    }

    public static void main(String[] a) throws Exception {
        String mode = a.length > 0 ? a[0] : "exit0";
        System.out.println("start " + mode);
        switch (mode) {
            case "exit0" -> System.exit(0);
            case "exit3" -> System.exit(3);
            case "hooks" -> { hook("a"); hook("b"); System.exit(5); }
            case "threadexit" -> {
                hook("t");
                Thread t = new Thread(() -> System.exit(4));
                t.start();
                t.join();
                Thread.sleep(5000);
                System.out.println("MUST-NOT-APPEAR");
            }
            case "haltnohook" -> { hook("nohook"); Runtime.getRuntime().halt(6); }
            case "exithook" -> {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    System.out.println("hook-exits");
                    Runtime.getRuntime().halt(9);
                }));
                System.exit(1);
            }
            case "runtimeexit" -> { hook("r"); Runtime.getRuntime().exit(7); }
            case "nested" -> {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    System.out.println("hook-calls-exit");
                    new Thread(() -> System.exit(2)).start();
                    try { Thread.sleep(300); } catch (InterruptedException e) { }
                    System.out.println("hook-done");
                }));
                System.exit(0);
            }
            case "flush" -> { System.out.print("unterminated"); System.exit(8); }
            case "dtor" -> {
                // normal return from main runs the hooks (the launcher path, not Shutdown.exit)
                hook("normal");
                System.out.println("main returns");
            }
            case "dup" -> {
                Thread h = new Thread(() -> System.out.println("hook dup ran"));
                Runtime rt = Runtime.getRuntime();
                rt.addShutdownHook(h);
                try { rt.addShutdownHook(h); } catch (Throwable t) { System.out.println("dup: " + t); }
                try { rt.addShutdownHook(null); } catch (Throwable t) { System.out.println("null add: " + t); }
                try { rt.removeShutdownHook(null); } catch (Throwable t) { System.out.println("null remove: " + t); }
                Thread live = new Thread(() -> { try { Thread.sleep(2000); } catch (InterruptedException e) { } });
                live.start();
                try { rt.addShutdownHook(live); } catch (Throwable t) { System.out.println("running: " + t); }
                System.out.println("remove unknown: " + rt.removeShutdownHook(new Thread(() -> { })));
                Thread gone = new Thread(() -> System.out.println("MUST-NOT-APPEAR removed hook"));
                rt.addShutdownHook(gone);
                System.out.println("remove known: " + rt.removeShutdownHook(gone));
                System.out.println("remove again: " + rt.removeShutdownHook(gone));
            }
            case "during" -> {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    try { Runtime.getRuntime().addShutdownHook(new Thread(() -> { })); } catch (Throwable t) { System.out.println("add during: " + t); }
                    try { Runtime.getRuntime().removeShutdownHook(Thread.currentThread()); } catch (Throwable t) { System.out.println("remove during: " + t); }
                }));
            }
            case "crosswait" -> {
                java.util.concurrent.CountDownLatch l = new java.util.concurrent.CountDownLatch(1);
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    try { l.await(); System.out.println("waiter released"); } catch (InterruptedException e) { }
                }));
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    System.out.println("counter counts down");
                    l.countDown();
                }));
            }
            case "deleteonexit" -> {
                java.io.File f = new java.io.File("/data/sbr5/sp/doe.tmp");
                f.createNewFile();
                f.deleteOnExit();
                Runtime.getRuntime().addShutdownHook(new Thread(() -> System.out.println("file exists in hook: " + f.exists())));
                System.exit(0);
            }
            case "uncaught" -> {
                hook("uncaught");
                throw new IllegalStateException("boom");
            }
            case "nondaemon" -> {
                hook("nondaemon");
                new Thread(() -> {
                    try { Thread.sleep(300); } catch (InterruptedException e) { }
                    System.out.println("non-daemon done");
                }).start();
                System.out.println("main ends");
            }
            case "exitinhook" -> {
                Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                    System.out.println("hook-exit-nonzero");
                    System.exit(3);
                }));
                System.exit(0);
            }
            default -> System.out.println("unknown mode");
        }
        System.out.println("MUST-NOT-APPEAR after " + mode);
    }
}
