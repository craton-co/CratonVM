public class ArgvProbe {
    public static void main(String[] args) {
        System.out.printf("ARGC %d%n", args.length);
        for (int i = 0; i < args.length; i++) {
            System.out.printf("ARG[%d] len=%d [%s]%n", i, args[i].length(), args[i]);
        }
    }
}
