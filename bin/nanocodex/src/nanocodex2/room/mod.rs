//! Typed client for the managed multiplayer-room protocol.
//!
//! The managed service deliberately exposes only room-scoped identities. This
//! module keeps account credentials and membership cookies in private,
//! redacted, zeroizing containers and rejects response fields outside the
//! public protocol.

mod api;
mod protocol;
mod socket;

pub(crate) use api::{
    AccountKey, CreatedRoom, JoinedRoom, RoomApi, RoomError, RoomInvitation, RoomMembership,
};
pub(crate) use protocol::{
    AgentErrorCode, MemberId, MessageId, ProtocolError, Ready, RoomCursor, RoomEventMessage,
    RoomId, RoomMember, RoomServerMessage, RoomTarget,
};
pub(crate) use socket::{RoomConnection, RoomEvents};
