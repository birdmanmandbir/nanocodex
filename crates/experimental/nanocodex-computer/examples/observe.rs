use nanocodex_computer::{
    ApplicationSelector, Computer, ComputerAction, ComputerOutput, ComputerPreview,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let bundle_id = arguments
        .first()
        .cloned()
        .unwrap_or_else(|| "com.apple.finder".to_owned());
    let open_preview = arguments.iter().any(|argument| argument == "--preview");
    let open_application = arguments.iter().any(|argument| argument == "--open");
    let print_json = arguments.iter().any(|argument| argument == "--json");
    let artifact_root = arguments
        .windows(2)
        .find(|pair| pair[0] == "--artifacts")
        .map(|pair| std::path::PathBuf::from(&pair[1]));
    let press = arguments
        .windows(2)
        .find(|pair| pair[0] == "--press")
        .map(|pair| pair[1].clone());
    let type_text = arguments
        .windows(2)
        .find(|pair| pair[0] == "--type-text")
        .map(|pair| pair[1].clone());
    let mut builder = Computer::builder().observe_human_input(false);
    if let Some(path) = artifact_root {
        builder = builder.artifact_root(path);
    }
    let (computer, _events) = builder.build()?;
    let preview = if open_preview {
        Some(ComputerPreview::spawn_and_open(&computer).await?)
    } else {
        None
    };
    if open_application {
        computer
            .execute(ComputerAction::OpenApplication {
                bundle_id: bundle_id.clone(),
            })
            .await?;
    }
    let result = computer
        .execute(ComputerAction::Attach {
            application: ApplicationSelector::BundleId(bundle_id),
            window_id: None,
        })
        .await?;
    let ComputerOutput::State { state } = result.output else {
        unreachable!("attach returns state");
    };
    println!(
        "{} — {}: {} accessibility elements, screenshot {} ({} ms)",
        state.application.name,
        state.window.title.as_deref().unwrap_or("untitled"),
        state.elements.len(),
        state.screenshot.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |image| image.path.display().to_string()
        ),
        result.elapsed_ms,
    );
    if print_json {
        println!("{}", serde_json::to_string_pretty(&state)?);
    }
    if let Some(text) = type_text {
        let result = computer
            .execute(ComputerAction::TypeText { text: text.clone() })
            .await?;
        let verified = match &result.output {
            ComputerOutput::State { state } => state
                .elements
                .iter()
                .filter_map(|element| element.value.as_deref())
                .any(|value| value.contains(&text)),
            _ => false,
        };
        println!(
            "typed {} characters and settled in {} ms; AX verification: {verified}",
            text.chars().count(),
            result.elapsed_ms
        );
        if print_json {
            println!("{}", serde_json::to_string_pretty(&result.output)?);
        }
    }
    if let Some(key) = press {
        let result = computer
            .execute(ComputerAction::PressKey {
                key: key.clone(),
                modifiers: Vec::new(),
            })
            .await?;
        println!("pressed {key} and settled in {} ms", result.elapsed_ms);
        if print_json {
            println!("{}", serde_json::to_string_pretty(&result.output)?);
        }
    }
    if let Some(preview) = preview {
        println!("preview: {} (press Return to close)", preview.url());
        tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)
        })
        .await??;
    }
    computer.stop();
    Ok(())
}
