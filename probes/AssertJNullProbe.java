import static org.assertj.core.api.Assertions.assertThat;

public class AssertJNullProbe {
    static class EqualsNull {
        public boolean equals(Object o) { return this == o || o == null; }
        public int hashCode() { return 7; }
        public String toString() { return "null"; }
    }
    public static void main(String[] a) {
        EqualsNull x = new EqualsNull();
        System.out.println("x.equals(null) = " + x.equals(null));
        System.out.println("org.assertj.core.util.Objects.areEqual = "
            + org.assertj.core.util.Objects.areEqual(x, null));
        try { assertThat(x).isEqualTo(null); System.out.println("assertThat(x).isEqualTo(null): PASS"); }
        catch (Throwable t) { System.out.println("assertThat(x).isEqualTo(null): FAIL " + t); }
        try { assertThat((Object) null).isEqualTo(null); System.out.println("assertThat(null).isEqualTo(null): PASS"); }
        catch (Throwable t) { System.out.println("assertThat(null).isEqualTo(null): FAIL " + t); }
    }
}
