    use super::*;

/// The gate screen's "card may be owed" exit is an edge only when asked:
    /// the default bytes keep it a decline, so the single-pass tier and the
    /// `Unsafe` arms are unchanged.
    #[test]
    fn the_card_edge_replaces_exactly_one_decline() {
        let g = super::super::VhRefGates {
            pre: 0x1000,
            post: 0x2000,
            floor: 0,
            mask: Some(0x40),
        };
        let mut plain = super::super::VhConstAsm::default();
        super::super::vh_ref_store_gate_screen_with(&mut plain, g, None, None);
        let mut edged = super::super::VhConstAsm::default();
        let mut edge = None;
        super::super::vh_ref_store_gate_screen_with(&mut edged, g, None, Some(&mut edge));
        assert_eq!(plain.b, edged.b, "same bytes, one jump retargeted");
        assert_eq!(plain.decline.len(), edged.decline.len() + 1);
        let edge = edge.expect("the edge");
        assert!(plain.decline.contains(&edge), "the retargeted jump is the owed-card decline");
    }
