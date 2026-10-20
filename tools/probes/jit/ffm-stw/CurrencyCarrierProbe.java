// SPDX-License-Identifier: Apache-2.0
// Check the fields populated on Currency instances fabricated by native code.
import java.util.Currency;

public final class CurrencyCarrierProbe {
    public static void main(String[] args) {
        for (String code : new String[] {"USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "CNY"}) {
            Currency currency = Currency.getInstance(code);
            System.out.println(code + " " + currency.getCurrencyCode() + " "
                    + currency.getDefaultFractionDigits() + " " + currency.getNumericCode());
        }
        System.out.println("PASS");
    }
}
