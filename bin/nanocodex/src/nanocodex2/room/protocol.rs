use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize};
use uuid::{Uuid, Variant};

pub(super) const MAX_ROOM_MESSAGE_BYTES: usize = 16 * 1024;
pub(super) const MAX_DISPLAY_NAME_BYTES: usize = 64;

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct RoomId(String);

impl RoomId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        let Some((uuid, capability)) = value.split_once('~') else {
            return Err(ProtocolError::InvalidRoomId);
        };
        let parsed = Uuid::parse_str(uuid).map_err(|_| ProtocolError::InvalidRoomId)?;
        if uuid.len() != 36
            || uuid.bytes().any(|byte| byte.is_ascii_uppercase())
            || parsed.get_version_num() != 7
            || parsed.get_variant() != Variant::RFC4122
            || !valid_token(capability)
        {
            return Err(ProtocolError::InvalidRoomId);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RoomId").field(&self.0).finish()
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for RoomId {
    type Err = ProtocolError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for RoomId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct MemberId(String);

impl MemberId {
    fn parse(value: String) -> Result<Self, ProtocolError> {
        if value.len() != 36
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'-'))
        {
            return Err(ProtocolError::InvalidMemberId);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MemberId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("MemberId").field(&self.0).finish()
    }
}

impl fmt::Display for MemberId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MemberId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct RoomCursor(String);

impl RoomCursor {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        let valid = value == "0"
            || (value.len() <= 19
                && value.starts_with(|byte: char| matches!(byte, '1'..='9'))
                && value.bytes().all(|byte| byte.is_ascii_digit()));
        if !valid {
            return Err(ProtocolError::InvalidCursor);
        }
        Ok(Self(value))
    }

    pub(crate) fn zero() -> Self {
        Self("0".to_owned())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RoomCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RoomCursor").field(&self.0).finish()
    }
}

impl fmt::Display for RoomCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RoomCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct MessageId(String);

impl MessageId {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, ProtocolError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
            })
        {
            return Err(ProtocolError::InvalidMessageId);
        }
        Ok(Self(value))
    }

    pub(crate) fn generate() -> Self {
        Self(format!("room-{}", Uuid::new_v4()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("MessageId").field(&self.0).finish()
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MessageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RoomTarget {
    Room,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomMember {
    pub(crate) id: MemberId,
    pub(crate) name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentErrorCode {
    Cancelled,
    Failed,
    Blocked,
    RateLimited,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RoomEventMessage {
    MemberJoined {
        member: RoomMember,
    },
    MemberMessage {
        id: MessageId,
        member: RoomMember,
        text: String,
        target: RoomTarget,
    },
    AgentMessage {
        id: MessageId,
        text: String,
        reply_to: RoomCursor,
    },
    AgentError {
        id: MessageId,
        code: AgentErrorCode,
        reply_to: RoomCursor,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ready {
    pub(crate) room_id: RoomId,
    pub(crate) member_id: MemberId,
    pub(crate) members: Vec<RoomMember>,
    pub(crate) online_member_ids: Vec<MemberId>,
    pub(crate) latest_cursor: RoomCursor,
    pub(crate) can_target_agent: bool,
    pub(crate) can_end_room: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum RoomServerMessage {
    Ready {
        room_id: RoomId,
        member_id: MemberId,
        members: Vec<RoomMember>,
        online_member_ids: Vec<MemberId>,
        latest_cursor: RoomCursor,
        can_target_agent: bool,
        can_end_room: bool,
    },
    RoomEvent {
        cursor: RoomCursor,
        created_at: u64,
        event: RoomEventMessage,
    },
    Accepted {
        id: MessageId,
        cursor: RoomCursor,
        replayed: bool,
    },
    ReplayPaused {
        cursor: RoomCursor,
        latest_cursor: RoomCursor,
    },
    Presence {
        online_member_ids: Vec<MemberId>,
    },
    Pong {
        #[serde(default)]
        nonce: Option<String>,
    },
    Error {
        code: String,
        message: String,
        #[serde(default)]
        id: Option<MessageId>,
    },
}

impl RoomServerMessage {
    pub(super) fn decode(encoded: &str) -> Result<Self, ProtocolError> {
        serde_json::from_str(encoded).map_err(|_| ProtocolError::InvalidServerMessage)
    }

    pub(super) fn into_ready(self) -> Result<Ready, ProtocolError> {
        let Self::Ready {
            room_id,
            member_id,
            members,
            online_member_ids,
            latest_cursor,
            can_target_agent,
            can_end_room,
        } = self
        else {
            return Err(ProtocolError::ReadyExpected);
        };
        Ok(Ready {
            room_id,
            member_id,
            members,
            online_member_ids,
            latest_cursor,
            can_target_agent,
            can_end_room,
        })
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum RoomClientCommand<'a> {
    Say {
        id: &'a MessageId,
        text: &'a str,
        target: RoomTarget,
    },
    Ack {
        cursor: &'a RoomCursor,
    },
    Ping {
        #[serde(skip_serializing_if = "Option::is_none")]
        nonce: Option<&'a str>,
    },
}

pub(super) fn validated_message_text(text: &str) -> Result<&str, ProtocolError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ProtocolError::EmptyMessage);
    }
    if text.len() > MAX_ROOM_MESSAGE_BYTES {
        return Err(ProtocolError::MessageTooLarge);
    }
    Ok(text)
}

pub(super) fn validated_display_name(name: &str) -> Result<&str, ProtocolError> {
    let name = name.trim();
    if name.is_empty() || name.len() > MAX_DISPLAY_NAME_BYTES || name.chars().any(char::is_control)
    {
        return Err(ProtocolError::InvalidDisplayName);
    }
    Ok(name)
}

pub(super) fn valid_token(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProtocolError {
    #[error("room id is invalid")]
    InvalidRoomId,
    #[error("room member id is invalid")]
    InvalidMemberId,
    #[error("room cursor is invalid")]
    InvalidCursor,
    #[error("room message id is invalid")]
    InvalidMessageId,
    #[error("room display name must be 1-64 UTF-8 bytes without control characters")]
    InvalidDisplayName,
    #[error("room message must not be empty")]
    EmptyMessage,
    #[error("room message exceeds 16 KiB")]
    MessageTooLarge,
    #[error("managed room server sent a malformed message")]
    InvalidServerMessage,
    #[error("managed room server did not begin with a ready message")]
    ReadyExpected,
}

#[cfg(test)]
mod tests {
    use super::{RoomCursor, RoomServerMessage};

    const ROOM: &str =
        "0198d214-0d9d-7a45-8a89-123456789abc~AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const MEMBER: &str = "0198d214-0d9d-7a45-8a89-123456789abc";

    #[test]
    fn parses_public_ready_shape() {
        let encoded = format!(
            r#"{{"type":"ready","room_id":"{ROOM}","member_id":"{MEMBER}","members":[{{"id":"{MEMBER}","name":"Ada"}}],"online_member_ids":["{MEMBER}"],"latest_cursor":"1","can_target_agent":true,"can_end_room":true}}"#,
        );
        let ready = RoomServerMessage::decode(&encoded)
            .unwrap()
            .into_ready()
            .unwrap();
        assert_eq!(ready.latest_cursor, RoomCursor::parse("1").unwrap());
    }

    #[test]
    fn rejects_private_or_unknown_server_fields() {
        let encoded = format!(
            r#"{{"type":"ready","room_id":"{ROOM}","member_id":"{MEMBER}","members":[],"online_member_ids":[],"latest_cursor":"0","can_target_agent":true,"can_end_room":true,"agent_id":"private"}}"#,
        );
        assert!(RoomServerMessage::decode(&encoded).is_err());
    }

    #[test]
    fn cursor_matches_managed_decimal_contract() {
        assert!(RoomCursor::parse("0").is_ok());
        assert!(RoomCursor::parse("1234567890123456789").is_ok());
        assert!(RoomCursor::parse("01").is_err());
        assert!(RoomCursor::parse("12345678901234567890").is_err());
    }
}
