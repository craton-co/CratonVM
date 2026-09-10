import net.bytebuddy.description.type.TypeDescription;
public class BbChain {
  public static void main(String[] a) throws Exception {
    Class<?> k = Class.forName(a.length>0?a[0]:"org.assertj.core.api.IntegerAssert");
    TypeDescription td = TypeDescription.ForLoadedType.of(k);
    System.out.println("BASE " + td.getName() + " tvars=" + td.getTypeVariables());
    TypeDescription.Generic g = td.getSuperClass();
    int i = 0;
    while (g != null && i++ < 8) {
      System.out.println("L" + i + " sort=" + g.getSort() + " str=" + g
        + " erasure=" + g.asErasure().getName()
        + " typeArgs=" + (g.getSort().isParameterized() ? g.getTypeArguments().toString() : "<none>")
        + " erasureTvars=" + g.asErasure().getTypeVariables());
      g = g.getSuperClass();
    }
  }
}
