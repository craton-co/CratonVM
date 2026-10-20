public final class Tiny {
    public static void main(String[] args) throws Exception {
        System.out.println("Begin");
        Thread.Builder.OfVirtual builder = Thread.ofVirtual();
        System.out.println("Got builder: " + builder);
        Thread thread = builder.start(() -> System.out.println("In vthread"));
        thread.join();
        System.out.println("Joined OK");
    }
}
