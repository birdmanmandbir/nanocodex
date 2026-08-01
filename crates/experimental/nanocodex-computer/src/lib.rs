#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]

mod driver;
mod error;
mod platform;
mod preview;
mod tool;
mod types;

pub use driver::{Computer, ComputerBuilder, ComputerControl, ComputerEvents, ComputerFrames};
pub use error::{ComputerBuildError, ComputerError};
pub use preview::ComputerPreview;
pub use tool::ComputerTool;
pub use types::{
    Application, ApplicationSelector, ComputerAction, ComputerActionResult, ComputerEvent,
    ComputerOutput, ComputerState, Element, ElementRef, InteractionTarget, InterventionReason,
    KeyModifier, MouseButton, Permission, Point, Rect, Screenshot, SettlePolicy, Window,
};

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nanocodex_tools::runtime::DynamicToolProvider;

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
    async fn computer_tool_is_deferred_with_typed_input_and_output() {
        let (builder, _) = crate::driver::recording_builder();
        let (computer, _events) = builder.build().unwrap();
        let tool = ComputerTool::from_computer(computer);
        assert!(tool.direct_tools().is_empty());
        assert!(tool.contains("computer"));
        assert!(!tool.contains("other"));
        let definitions = tool.available_definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name(), "computer");
        assert!(definitions[0].parameters().is_some());
        assert!(definitions[0].output_schema().is_some());
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
