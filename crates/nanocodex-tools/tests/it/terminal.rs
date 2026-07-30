#[cfg(unix)]
#[tokio::test]
async fn public_runtime_terminal_capability_controls_a_model_pty() {
    use nanocodex_tools::{
        ToolContext,
        contract::{DEFAULT_TOOL_OUTPUT_TOKENS, ToolInput},
        runtime::ToolRuntime,
        terminal::TerminalEvent,
    };
    use serde_json::{json, value::to_raw_value};

    let runtime = ToolRuntime::new("/", None, None);
    let terminals = runtime.terminals();
    let mut events = terminals.subscribe();
    let context = ToolContext::new(
        "test-model",
        "test-session",
        "call-public-terminal",
        &[],
        DEFAULT_TOOL_OUTPUT_TOKENS,
    );
    let started = runtime
        .execute_tool(
            "exec_command",
            ToolInput::Function(
                to_raw_value(&json!({
                    "cmd": "stty -echo; printf ready; read value; printf 'got:%s' \"$value\"",
                    "tty": true,
                    "yield_time_ms": 250
                }))
                .unwrap(),
            ),
            context,
        )
        .await;
    let session_id = started.code_mode_value()["session_id"]
        .as_i64()
        .expect("yielded command should expose its model session ID");
    let opened = loop {
        if let Some(TerminalEvent::Opened(info)) = events.recv().await.unwrap() {
            break info;
        }
    };
    assert_eq!(opened.call_id.as_ref(), "call-public-terminal");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut output = Vec::new();
        loop {
            if let Some(TerminalEvent::Output { bytes, .. }) = events.recv().await.unwrap() {
                output.extend_from_slice(&bytes);
                if output.ends_with(b"ready") {
                    break;
                }
            }
        }
    })
    .await
    .expect("PTY should reach its input prompt");
    terminals.write(opened.id, b"hello\n").await.unwrap();
    let completed = runtime
        .execute_tool(
            "write_stdin",
            ToolInput::Function(
                to_raw_value(&json!({
                    "session_id": session_id,
                    "yield_time_ms": 5_000
                }))
                .unwrap(),
            ),
            ToolContext::new(
                "test-model",
                "test-session",
                "call-public-poll",
                &[],
                DEFAULT_TOOL_OUTPUT_TOKENS,
            ),
        )
        .await;
    assert_eq!(completed.code_mode_value()["exit_code"], 0);
    assert_eq!(
        terminals.snapshot(opened.id).unwrap().output.as_ref(),
        b"readygot:hello"
    );
}
