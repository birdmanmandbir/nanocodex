use nanocodex_computer::{
    ApplicationSelector, Computer, ComputerAction, ComputerOutput, ComputerPreview,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bundle_id = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "com.apple.finder".to_owned());
    let open_preview = std::env::args().any(|argument| argument == "--preview");
    let (computer, _events) = Computer::builder().observe_human_input(false).build()?;
    let preview = if open_preview {
        Some(ComputerPreview::spawn_and_open(&computer).await?)
    } else {
        None
    };
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
        "{} — {}: {} accessibility elements, screenshot {}",
        state.application.name,
        state.window.title.as_deref().unwrap_or("untitled"),
        state.elements.len(),
        state.screenshot.as_ref().map_or_else(
            || "unavailable".to_owned(),
            |image| image.path.display().to_string()
        ),
    );
    if let Some(preview) = preview {
        println!("preview: {} (press Return to close)", preview.url());
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
    }
    computer.stop();
    Ok(())
}
