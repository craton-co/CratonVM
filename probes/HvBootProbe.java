public class HvBootProbe {
  public static void main(String[] args) {
    try {
      jakarta.validation.Validation.buildDefaultValidatorFactory();
      System.out.println("OK");
    }
    catch (Throwable t) {
      t.printStackTrace(System.out);
    }
  }
}
