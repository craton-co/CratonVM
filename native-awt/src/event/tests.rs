use super::*;

#[test]
fn event_id_constants_match_java() {
    // Spot-check a few well-known values
    assert_eq!(event_id::KEY_PRESSED, 401);
    assert_eq!(event_id::MOUSE_CLICKED, 500);
    assert_eq!(event_id::WINDOW_CLOSING, 201);
    assert_eq!(event_id::ACTION_PERFORMED, 1001);
    assert_eq!(event_id::PAINT, 800);
    assert_eq!(event_id::INVOCATION_DEFAULT, 1200);
}

#[test]
fn vk_constants_match_java() {
    assert_eq!(vk::VK_ENTER, 10);
    assert_eq!(vk::VK_ESCAPE, 27);
    assert_eq!(vk::VK_A, 65);
    assert_eq!(vk::VK_F1, 112);
}

#[test]
fn modifier_masks() {
    assert_eq!(modifiers::SHIFT_DOWN_MASK, 64);
    assert_eq!(modifiers::CTRL_DOWN_MASK, 128);
    assert_eq!(modifiers::ALT_DOWN_MASK, 512);
    assert_eq!(modifiers::BUTTON1_DOWN_MASK, 1024);
}

#[test]
fn mouse_event_creation() {
    let evt = AwtEvent::mouse(
        event_id::MOUSE_PRESSED,
        PeerId(42),
        1000,
        100,
        200,
        BUTTON1,
        1,
        modifiers::BUTTON1_DOWN_MASK,
    );
    assert_eq!(evt.id, event_id::MOUSE_PRESSED);
    assert_eq!(evt.source_peer_id, PeerId(42));
    if let AwtEventData::Mouse {
        x,
        y,
        button,
        click_count,
        modifiers: mods,
        scroll_amount,
    } = &evt.data
    {
        assert_eq!(*x, 100);
        assert_eq!(*y, 200);
        assert_eq!(*button, BUTTON1);
        assert_eq!(*click_count, 1);
        assert_eq!(*mods, modifiers::BUTTON1_DOWN_MASK);
        assert_eq!(*scroll_amount, 0);
    } else {
        panic!("expected Mouse variant");
    }
}

#[test]
fn key_event_creation() {
    let evt = AwtEvent::key(event_id::KEY_PRESSED, PeerId(1), 500, vk::VK_A, 'a', 0);
    assert_eq!(evt.id, event_id::KEY_PRESSED);
    if let AwtEventData::Key {
        key_code,
        key_char,
        modifiers: mods,
    } = &evt.data
    {
        assert_eq!(*key_code, vk::VK_A);
        assert_eq!(*key_char, 'a');
        assert_eq!(*mods, 0);
    } else {
        panic!("expected Key variant");
    }
}

#[test]
fn invocation_event_creation() {
    let evt = AwtEvent::invocation(PeerId(0), 999, 77);
    assert_eq!(evt.id, event_id::INVOCATION_DEFAULT);
    if let AwtEventData::Invocation { callback_id } = &evt.data {
        assert_eq!(*callback_id, 77);
    } else {
        panic!("expected Invocation variant");
    }
}

#[test]
fn paint_event_detection() {
    let paint = AwtEvent::paint(event_id::PAINT, PeerId(1), 0, 0, 0, 100, 100);
    let update = AwtEvent::paint(event_id::UPDATE, PeerId(1), 0, 0, 0, 100, 100);
    let click = AwtEvent::mouse(event_id::MOUSE_CLICKED, PeerId(1), 0, 0, 0, BUTTON1, 1, 0);
    assert!(paint.is_paint());
    assert!(update.is_paint());
    assert!(!click.is_paint());
}

#[test]
fn component_event() {
    let evt = AwtEvent::component(
        event_id::COMPONENT_RESIZED,
        PeerId(5),
        123,
        10,
        20,
        800,
        600,
    );
    if let AwtEventData::Component {
        x,
        y,
        width,
        height,
    } = &evt.data
    {
        assert_eq!((*x, *y, *width, *height), (10, 20, 800, 600));
    } else {
        panic!("expected Component variant");
    }
}

#[test]
fn window_event() {
    let evt = AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(3), 456);
    assert_eq!(evt.id, event_id::WINDOW_CLOSING);
    assert!(matches!(evt.data, AwtEventData::Window));
}

#[test]
fn action_event() {
    let evt = AwtEvent::action(PeerId(7), 789, "clicked".to_string());
    assert_eq!(evt.id, event_id::ACTION_PERFORMED);
    if let AwtEventData::Action { command } = &evt.data {
        assert_eq!(command, "clicked");
    } else {
        panic!("expected Action variant");
    }
}

#[test]
fn mouse_wheel_event() {
    let evt = AwtEvent::mouse_wheel(PeerId(2), 100, 50, 60, 0, 3);
    assert_eq!(evt.id, event_id::MOUSE_WHEEL);
    if let AwtEventData::Mouse { scroll_amount, .. } = &evt.data {
        assert_eq!(*scroll_amount, 3);
    } else {
        panic!("expected Mouse variant");
    }
}

#[test]
fn peer_id_equality() {
    let a = PeerId(42);
    let b = PeerId(42);
    let c = PeerId(99);
    assert_eq!(a, b);
    assert_ne!(a, c);
}
