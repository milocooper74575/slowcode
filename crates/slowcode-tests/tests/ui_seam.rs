//! The public UI event seam projects committed facts for renderers.

use slowcode_kernel::{
    session::{EventKind, NewEvent, Session, TurnId},
    ui::UiEventKind,
};

#[test]
fn public_ui_event_stream_projects_turn_start() {
    let mut session = Session::new("ui-seam").expect("session opens");
    let mut events = session
        .attach_ui_events()
        .expect("observer attaches before events");
    let turn = TurnId::new(1).expect("turn id is valid");
    session
        .append(NewEvent::log(EventKind::turn_start(turn)))
        .expect("turn start commits");

    let event = events.try_recv().expect("turn start is projected");
    assert_eq!(event.seq, 0);
    assert!(matches!(event.kind, UiEventKind::TurnStart { turn: 1 }));
    assert!(events.try_recv().is_err());
}
