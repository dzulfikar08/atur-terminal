use super::*;
use muxy_protocol::ClientKind;

/// Longer than the server's handover time: the previous typist is quiet by then.
const HANDOVER: Duration = Duration::from_millis(1000);

const WIDE: Size = Size {
    cols: 100,
    rows: 30,
};
const MID: Size = Size { cols: 70, rows: 22 };
const NARROW: Size = Size { cols: 40, rows: 15 };

fn identified(fixture: &Fixture, kind: ClientKind) -> TestResult<Connection> {
    let connection = fixture.connect()?;
    connection.client.identify(kind)?;
    Ok(connection)
}

/// Applies frames until the grid has `wanted`.
fn follow(connection: &Connection, attachment: &mut Attachment, wanted: Size) -> TestResult {
    let deadline = Instant::now() + TIMEOUT;
    while attachment.grid.size != wanted {
        if Instant::now() > deadline {
            return Err(format!(
                "the grid stayed {:?}, wanted {wanted:?}",
                attachment.grid.size
            )
            .into());
        }
        let frame = connection.next_frame(attachment.channel)?;
        attachment.grid.apply(&frame);
        connection.client.ack(attachment.channel, frame.seq)?;
    }
    Ok(())
}

/// Takes in everything that is on its way, then expects the size not to have changed.
fn stays(connection: &Connection, attachment: &mut Attachment, size: Size) -> TestResult {
    connection.quiet(attachment)?;
    assert_eq!(attachment.grid.size, size);
    Ok(())
}

#[test]
fn the_session_follows_the_client_that_types_and_watchers_never_resize_it() -> TestResult {
    let fixture = Fixture::new()?;
    let desktop = identified(&fixture, ClientKind::Desktop)?;
    let tui = identified(&fixture, ClientKind::Tui)?;
    let phone = identified(&fixture, ClientKind::Mobile)?;
    let session = fixture.create(&desktop.client)?;

    // The first client that is not a watcher sizes the session as it attaches; the
    // others, a terminal app and a phone, find it as it is.
    let mut on_desktop = desktop.client.attach(session.id, WIDE)?;
    assert_eq!(on_desktop.grid.size, WIDE);
    let mut on_tui = tui.client.attach(session.id, MID)?;
    let mut on_phone = phone.client.attach(session.id, NARROW)?;
    assert_eq!(on_tui.grid.size, WIDE);
    assert_eq!(on_phone.grid.size, WIDE);

    // A phone only watches: it tells its grid and is answered with the screen it has.
    phone.client.resize(on_phone.channel, NARROW)?;
    stays(&phone, &mut on_phone, WIDE)?;
    stays(&desktop, &mut on_desktop, WIDE)?;

    // The desktop types and owns the keyboard. The terminal app's grid is only told.
    desktop
        .client
        .send_input(on_desktop.channel, b"printf 'FROM_%s\\n' DESKTOP\r")?;
    desktop.frame_containing(&mut on_desktop, "FROM_DESKTOP")?;
    tui.client.resize(on_tui.channel, MID)?;
    stays(&tui, &mut on_tui, WIDE)?;
    stays(&desktop, &mut on_desktop, WIDE)?;

    // The terminal app types once the desktop has been quiet: the session takes its size
    // for everyone.
    thread::sleep(HANDOVER);
    tui.client
        .send_input(on_tui.channel, b"printf 'FROM_%s\\n' TUI\r")?;
    follow(&tui, &mut on_tui, MID)?;
    follow(&desktop, &mut on_desktop, MID)?;
    follow(&phone, &mut on_phone, MID)?;

    // Typing at the same moment does not move the keyboard.
    phone
        .client
        .send_input(on_phone.channel, b"printf 'FROM_%s\\n' EARLY\r")?;
    tui.frame_containing(&mut on_tui, "FROM_EARLY")?;
    assert_eq!(on_tui.grid.size, MID);
    stays(&desktop, &mut on_desktop, MID)?;

    // The phone types later and takes the keyboard with the size it told.
    thread::sleep(HANDOVER);
    phone
        .client
        .send_input(on_phone.channel, b"printf 'FROM_%s\\n' PHONE\r")?;
    follow(&phone, &mut on_phone, NARROW)?;
    follow(&desktop, &mut on_desktop, NARROW)?;
    follow(&tui, &mut on_tui, NARROW)?;

    // While the phone has the keyboard, no other client sizes the session.
    desktop.client.resize(on_desktop.channel, WIDE)?;
    stays(&desktop, &mut on_desktop, NARROW)?;
    stays(&phone, &mut on_phone, NARROW)?;

    // When the phone leaves, the clients that are not watching size it again.
    phone.client.detach(on_phone.channel)?;
    let deadline = Instant::now() + TIMEOUT;
    while on_desktop.grid.size != WIDE {
        assert!(
            Instant::now() < deadline,
            "the desktop never sized it again"
        );
        desktop.client.resize(on_desktop.channel, WIDE)?;
        desktop.quiet(&mut on_desktop)?;
    }
    follow(&tui, &mut on_tui, WIDE)?;
    Ok(())
}

#[test]
fn a_phone_that_attaches_alone_does_not_resize_the_session_until_it_types() -> TestResult {
    let fixture = Fixture::new()?;
    let phone = identified(&fixture, ClientKind::Mobile)?;
    let session = fixture.create(&phone.client)?;
    let mut on_phone = phone.client.attach(session.id, NARROW)?;
    assert_eq!(
        on_phone.grid.size, SIZE,
        "the session keeps the size it has"
    );
    phone.client.resize(on_phone.channel, NARROW)?;
    stays(&phone, &mut on_phone, SIZE)?;
    phone
        .client
        .send_input(on_phone.channel, b"printf 'FROM_%s\\n' PHONE\r")?;
    follow(&phone, &mut on_phone, NARROW)?;
    if !text(&on_phone.grid).contains("FROM_PHONE") {
        phone.frame_containing(&mut on_phone, "FROM_PHONE")?;
    }
    Ok(())
}
