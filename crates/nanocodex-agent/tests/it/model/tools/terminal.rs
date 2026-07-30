use super::*;

#[cfg(unix)]
#[tokio::test]
async fn agent_terminal_capability_controls_its_private_tool_runtime() -> Result<()> {
    use nanocodex_agent::tools::terminal::{TerminalEvent, TerminalSize};

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("ws://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = accept_async(stream).await?;
        assert_warmup(&next_json(&mut socket).await?);
        send_warmup(&mut socket, "resp-warmup").await?;

        let generation = next_json(&mut socket).await?;
        assert_eq!(generation["previous_response_id"], "resp-warmup");
        send_json(
            &mut socket,
            completed_response(
                "resp-terminal",
                &[json!({
                    "type": "custom_tool_call",
                    "call_id": "call-terminal-cell",
                    "name": "exec",
                    "input": concat!(
                        "const result = await tools.exec_command({",
                        "cmd: \"stty -echo; printf ready; read value; printf 'got:%s' \\\"$value\\\"\", ",
                        "tty: true, yield_time_ms: 5_000",
                        "}); text(result.output);"
                    )
                })],
            ),
        )
        .await?;

        let continuation = next_json(&mut socket).await?;
        let output = continuation["input"][0]["output"].to_string();
        assert!(output.contains("readygot:hello"), "{output}");
        send_final(&mut socket, "resp-final").await
    });

    let workspace = temporary_workspace("agent-terminal-control")?;
    let openai = OpenAi::builder("test-key")
        .websocket_url(endpoint)
        .build()?;
    let (agent, events) = Nanocodex::builder(openai)
        .thinking(Thinking::Low)
        .workspace(&workspace)
        .session_id(test_session_id())
        .build()?;
    let terminals = agent.terminals();
    let mut terminal_events = terminals.subscribe();
    let turn = agent.prompt("run an interactive terminal command").await?;

    let opened = timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(TerminalEvent::Opened(info)) = terminal_events.recv().await.unwrap() {
                break info;
            }
        }
    })
    .await
    .map_err(|_| eyre!("agent PTY did not open"))?;
    assert!(!opened.call_id.is_empty());
    terminals
        .resize(opened.id, TerminalSize::new(30, 90))
        .await?;
    timeout(std::time::Duration::from_secs(5), async {
        let mut output = Vec::new();
        loop {
            if let Some(TerminalEvent::Output { bytes, .. }) = terminal_events.recv().await.unwrap()
            {
                output.extend_from_slice(&bytes);
                if output.ends_with(b"ready") {
                    break;
                }
            }
        }
    })
    .await
    .map_err(|_| eyre!("agent PTY did not reach its input prompt"))?;
    terminals.write(opened.id, b"hello\n").await?;

    assert_eq!(turn.result().await?.final_message(), "done");
    let snapshot = terminals.snapshot(opened.id)?;
    assert_eq!(snapshot.output.as_ref(), b"readygot:hello");
    assert_eq!(snapshot.exit_code, Some(0));

    drop((agent, events));
    timeout(std::time::Duration::from_secs(5), server)
        .await
        .map_err(|_| eyre!("mock Responses server did not finish"))???;
    std::fs::remove_dir_all(workspace)?;
    Ok(())
}
