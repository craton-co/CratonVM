public class TestFieldDebug {
    int x;
    int y;

    public TestFieldDebug(int x, int y) {
        this.x = x;
        this.y = y;
    }

    public static void main(String[] args) {
        System.out.println("creating object");
        TestFieldDebug t = new TestFieldDebug(10, 20);
        System.out.println("reading x");
        System.out.println("x=" + t.x);
        System.out.println("y=" + t.y);
        System.out.println("DONE");
    }
}
