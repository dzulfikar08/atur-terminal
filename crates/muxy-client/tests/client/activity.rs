use super::*;
use muxy_protocol::{AgentState, SessionId};

fn wait_state(
    client: &Client,
    session: SessionId,
    expected: AgentState,
) -> TestResult<muxy_protocol::ActivitySnapshot> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        let snapshot = client.activity()?;
        if snapshot
            .agents
            .iter()
            .any(|agent| agent.session == session && agent.state == expected)
        {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(format!("agent did not reach {expected:?}: {:?}", client.activity()?).into())
}

#[test]
fn omp_osc_titles_drive_activity_and_notifications_without_screen_markers() -> TestResult {
    use muxy_protocol::{ActivityKind, AgentProvider};

    let fixture = Fixture::new()?;
    let connection = fixture.connect()?;
    let session = fixture.create(&connection.client)?;
    let handle = fixture.registry.handle(session.id).ok_or("session")?;
    handle.send(muxy_server::SessionCommand::Input(
        b"/bin/bash -c 'exec -a omp /bin/cat'\n".to_vec(),
    ))?;
    wait_state(&connection.client, session.id, AgentState::Idle)?;
    for (title, expected, event) in [
        ("π ⠋ Inspect files", AgentState::Working, None),
        (
            "π ! Inspect files",
            AgentState::Blocked,
            Some(ActivityKind::Attention),
        ),
        (
            "π ⠙ Inspect files",
            AgentState::Working,
            Some(ActivityKind::Attention),
        ),
        (
            "π > Inspect files",
            AgentState::Done,
            Some(ActivityKind::Completed),
        ),
    ] {
        handle.send(muxy_server::SessionCommand::Input(
            format!("\x1b]0;{title}\x07\n").into_bytes(),
        ))?;
        let snapshot = wait_state(&connection.client, session.id, expected)?;
        assert_eq!(snapshot.agents[0].provider, AgentProvider::OhMyPi);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            event.into_iter().collect::<Vec<_>>(),
        );
    }
    connection.client.end_session(session.id)?;
    Ok(())
}
