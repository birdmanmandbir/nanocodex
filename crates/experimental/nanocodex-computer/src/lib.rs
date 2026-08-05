#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod driver;
mod error;
mod pip;
mod platform;
mod preview;
mod tool;
mod types;

pub use driver::{Computer, ComputerBuilder, ComputerControl, ComputerEvents, ComputerFrames};
pub use error::{ComputerBuildError, ComputerError};
pub use pip::ComputerPip;
pub use preview::ComputerPreview;
pub use tool::ComputerTool;
pub use types::{
    AccessibilityUpdate, Application, ApplicationSelector, CapturedImage, ComputerAction,
    ComputerActionResult, ComputerEvent, ComputerFrame, ComputerFramePhase, ComputerObservation,
    ComputerOutput, Element, ElementRef, InteractionTarget, InterventionReason, KeyModifier,
    MouseButton, Permission, Point, Rect, ScreenshotArtifact, SettlePolicy, Window,
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nanocodex_tools::{
        ToolContext, ToolDefinition,
        contract::DEFAULT_TOOL_OUTPUT_TOKENS,
        runtime::{DynamicToolProvider, ToolRuntime, Tools},
    };

    use super::*;

    #[tokio::test]
    async fn serializes_actions_and_keeps_events_optional() {
        let (builder, actions) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let first = computer
            .execute(ComputerAction::Wait { milliseconds: 1 })
            .await
            .unwrap();
        let second = computer
            .execute(ComputerAction::TypeText {
                text: "hello".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(actions.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn intervention_rejects_actions_until_resume() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let control = computer.control();
        control.intervene(InterventionReason::HumanInput);
        assert!(matches!(
            computer.execute(ComputerAction::ListApplications).await,
            Err(ComputerError::Paused)
        ));
        control.resume();
        computer
            .execute(ComputerAction::ListApplications)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stopped_sessions_cannot_be_resumed() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let control = computer.control();
        control.stop();
        control.pause();
        control.intervene(InterventionReason::Caller("late".to_owned()));
        control.resume();
        assert!(matches!(
            computer.execute(ComputerAction::ListApplications).await,
            Err(ComputerError::Stopped)
        ));
    }

    #[tokio::test]
    async fn slow_event_observers_receive_an_explicit_lag_marker() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, mut events) = builder.build().unwrap();
        for _ in 0..140 {
            computer
                .execute(ComputerAction::Wait { milliseconds: 0 })
                .await
                .unwrap();
        }
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::Lagged { skipped }) if skipped > 0
        ));
    }

    #[tokio::test]
    async fn lifecycle_subscribers_have_independent_cursors() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, mut original) = builder.build().unwrap();
        let mut subscribed = computer.events();
        computer
            .execute(ComputerAction::ListApplications)
            .await
            .unwrap();

        while !matches!(
            original.recv().await,
            Some(ComputerEvent::ActionCompleted { .. })
        ) {}
        while !matches!(
            subscribed.recv().await,
            Some(ComputerEvent::ActionCompleted { .. })
        ) {}
    }

    #[tokio::test]
    async fn target_change_precedes_first_frame_and_is_not_repeated() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, mut events) = builder.build().unwrap();
        computer
            .execute(ComputerAction::Observe { screenshot: true })
            .await
            .unwrap();

        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::SessionStarted { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::ActionStarted { sequence: 1, .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::TargetChanged { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::Frame { sequence: 1, .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::ActionCompleted { .. })
        ));

        computer
            .execute(ComputerAction::Observe { screenshot: true })
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::ActionStarted { sequence: 2, .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::Frame { sequence: 2, .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(ComputerEvent::ActionCompleted { .. })
        ));
    }

    #[tokio::test]
    async fn computer_tool_has_typed_input_and_output() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let tool = ComputerTool::from_computer(computer);
        assert!(tool.contains("computer"));
        assert!(!tool.contains("other"));
        assert!(tool.available_definitions().is_empty());
        let definitions = tool.direct_tools();
        assert_eq!(definitions.len(), 1);
        let definition = definitions[0].definition();
        assert_eq!(definition.name(), "computer");
        assert!(definition.parameters().is_some());
        assert!(definition.output_schema().is_some());
    }

    #[tokio::test]
    async fn computer_tool_is_described_and_callable_from_code_mode() {
        let (builder, actions) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let tools = Tools::builder()
            .without_defaults()
            .provider(ComputerTool::from_computer(computer))
            .build()
            .unwrap();
        let runtime = ToolRuntime::new(".", None, None).with_tools(&tools);
        let specs = runtime.model_specs("test-session");
        let exec = specs
            .iter()
            .find(|definition| definition.name() == "exec")
            .unwrap();
        assert!(
            exec.description()
                .contains("declare const tools: { computer(args:")
        );
        assert_eq!(
            specs.iter().map(ToolDefinition::name).collect::<Vec<_>>(),
            ["exec", "wait"]
        );

        let execution = runtime
            .execute_code(
                r#"
const result = await tools.computer({ action: "wait", milliseconds: 0 });
text(result.sequence);
"#,
                ToolContext::new(
                    "test-model",
                    "test-session",
                    "test-call",
                    &[],
                    DEFAULT_TOOL_OUTPUT_TOKENS,
                ),
            )
            .await;
        assert!(execution.success);
        assert_eq!(execution.nested_calls.len(), 1);
        assert_eq!(execution.nested_calls[0].name, "computer");
        assert_eq!(
            actions.lock().unwrap().as_slice(),
            [ComputerAction::Wait { milliseconds: 0 }]
        );
    }

    #[test]
    fn rectangle_center_is_stable() {
        assert_eq!(
            Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            }
            .center(),
            Point { x: 25.0, y: 40.0 }
        );
        assert_eq!(SettlePolicy::default().timeout, Duration::from_secs(5));
    }
}
