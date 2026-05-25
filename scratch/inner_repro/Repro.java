public class Repro {
    static class Inner1 {}
    static class Inner2 {}
    static class Inner3 {}

    static class NoInner {}

    static class OnlyAnonymous {
        Runnable r = new Runnable() { public void run() {} };
    }

    public static void main(String[] args) throws Exception {
        Class<?> minecraftMain = Class.forName("net.minecraft.bundler.Main");
        Class<?>[] mcInners = minecraftMain.getDeclaredClasses();
        System.out.println("inners = " + mcInners.length);
        for (Class<?> c : mcInners) {
            System.out.println("  " + c.getName());
        }

        Class<?> noInner = NoInner.class;
        Class<?>[] none = noInner.getDeclaredClasses();
        System.out.println("noInner.getDeclaredClasses().length = " + none.length);
        System.out.println("noInner.getDeclaredClasses() != null: " + (none != null));

        Class<?> anon = OnlyAnonymous.class;
        Class<?>[] anonResult = anon.getDeclaredClasses();
        System.out.println("OnlyAnonymous.getDeclaredClasses().length = " + anonResult.length);
        for (Class<?> c : anonResult) {
            System.out.println("  " + c.getName());
        }

        Class<?> repro = Repro.class;
        Class<?>[] reproInners = repro.getDeclaredClasses();
        System.out.println("Repro.getDeclaredClasses().length = " + reproInners.length);
        for (Class<?> c : reproInners) {
            System.out.println("  " + c.getName());
        }
    }
}
