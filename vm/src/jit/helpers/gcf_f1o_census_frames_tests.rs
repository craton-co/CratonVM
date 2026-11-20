//! gcf f1/o: the census frames line's pure parts.
use super::{oldmark_census_frame_token, oldmark_census_tier};

#[test]
fn gcf_f1o_census_tier_names_every_body() {
    assert_eq!(oldmark_census_tier(true, false), "ir");
    assert_eq!(oldmark_census_tier(true, true), "ir-osr");
    assert_eq!(oldmark_census_tier(false, false), "sp");
    assert_eq!(oldmark_census_tier(false, true), "sp-osr");
}

#[test]
fn gcf_f1o_census_frame_token_shape() {
    assert_eq!(
        oldmark_census_frame_token(
            "Probe.chainCatch:(Ljava/lang/ref/WeakReference;I)V",
            "sp",
            12
        ),
        "Probe.chainCatch:(Ljava/lang/ref/WeakReference;I)V@sp:12"
    );
    assert_eq!(
        oldmark_census_frame_token("A.b:()V", "ir", -1),
        "A.b:()V@ir:-1"
    );
}
