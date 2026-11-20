use super::{native_pin_balance, NativePinBalance};

#[test]
fn the_verdicts() {
    assert_eq!(native_pin_balance(3, 3, false), NativePinBalance::Balanced);
    assert_eq!(native_pin_balance(3, 3, true), NativePinBalance::Balanced);
    assert_eq!(native_pin_balance(3, 5, false), NativePinBalance::Leaked(2));
    assert_eq!(
        native_pin_balance(3, 4, true),
        NativePinBalance::ExceptionHandoff(1)
    );
    assert_eq!(
        native_pin_balance(3, 1, false),
        NativePinBalance::Underflow(2)
    );
    assert_eq!(
        native_pin_balance(3, 0, true),
        NativePinBalance::Underflow(3)
    );
    assert_eq!(native_pin_balance(0, 0, false), NativePinBalance::Balanced);
}
