public class ExceptionId {
    public static void main(String[] args) {
        try {
        } catch (ClassCastException e) {
        }
        try {
            String s = "abc";
            char c = s.charAt(9);
        } catch (StringIndexOutOfBoundsException e) {
            System.out.println("SIOOBE: " + e.getMessage());
        }
    }
}
