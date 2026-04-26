public class TestFields {
    int x;
    String name;

    TestFields(int x, String name) {
        this.x = x;
        this.name = name;
    }

    public static void main(String[] args) {
        TestFields t = new TestFields(42, "hello");
        System.out.println("x=" + t.x);
        System.out.println("name=" + t.name);
        System.out.println("DONE");
    }
}
