@AcpTag(name = "a", type = AcpRef.class)
@AcpTag(name = "b", type = String.class)
public class AcpTarget {

    @AcpFieldValue("field")
    public void withFieldValue() {
    }

    @AcpTag(name = "m1", type = AcpRef.class)
    @AcpTag(name = "m2", type = Integer.class)
    public void repeatedOnMethod() {
    }
}
