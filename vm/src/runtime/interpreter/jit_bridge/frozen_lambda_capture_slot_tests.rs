/// A frozen lambda's reference captures name their objects by the
/// implementation's slot; a `long` takes two, a primitive names none.
#[test]
fn captures_fill_the_implementations_leading_slots() {
    let (classes, objects) =
        super::lambda_capture_slots(&['L', 'J', 'I', 'L'], |i| Some((10 + i as u32, 0x100 * (i + 1))));
    assert_eq!(classes, vec![Some(10), None, None, None, Some(13)]);
    assert_eq!(objects, vec![Some(0x100), None, None, None, Some(0x400)]);
}
