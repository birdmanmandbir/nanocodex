// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Lossless forwarding for active and retiring Nanocodex event streams.

use crate::{engine::ManagedAgentEvents, tui::pane::PaneId};
use nanocodex::agent::events::AgentEvent;
use tokio::sync::mpsc;

pub(crate) enum ForwardedAgentEvent {
    Event {
        pane: PaneId,
        session_id: String,
        generation: u64,
        event: AgentEvent,
    },
    Closed {
        pane: PaneId,
        session_id: String,
        generation: u64,
    },
}

pub(crate) fn forward(
    pane: PaneId,
    generation: u64,
    mut events: ManagedAgentEvents,
    sender: mpsc::UnboundedSender<ForwardedAgentEvent>,
) {
    let session_id = events.identity().session_id().to_owned();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if sender
                .send(ForwardedAgentEvent::Event {
                    pane,
                    session_id: session_id.clone(),
                    generation,
                    event,
                })
                .is_err()
            {
                return;
            }
        }
        drop(sender.send(ForwardedAgentEvent::Closed {
            pane,
            session_id,
            generation,
        }));
    });
}
