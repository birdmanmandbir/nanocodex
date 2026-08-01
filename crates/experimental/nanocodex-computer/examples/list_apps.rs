use nanocodex_computer::{Computer, ComputerAction, ComputerOutput};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (computer, _events) = Computer::builder().observe_human_input(false).build()?;
    let result = computer.execute(ComputerAction::ListApplications).await?;
    let ComputerOutput::Applications {
        applications,
        windows,
    } = result.output
    else {
        unreachable!("list_applications returns discovery output");
    };
    for application in applications {
        let window_count = windows
            .iter()
            .filter(|window| window.pid == application.pid)
            .count();
        println!(
            "{}\t{}\t{}\t{window_count} window(s)",
            application.pid,
            application.bundle_id.as_deref().unwrap_or("-"),
            application.name,
        );
    }
    Ok(())
}
