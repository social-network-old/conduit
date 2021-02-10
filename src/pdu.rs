use crate::Error;
use ruma::{
    events::{
        pdu::EventHash, room::member::MemberEventContent, AnyEvent, AnyRoomEvent, AnyStateEvent,
        AnyStrippedStateEvent, AnySyncRoomEvent, AnySyncStateEvent, EventType, StateEvent,
    },
    serde::{to_canonical_value, CanonicalJsonObject, CanonicalJsonValue, Raw},
    EventId, RoomId, RoomVersionId, ServerName, ServerSigningKeyId, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{cmp::Ordering, collections::BTreeMap, convert::TryFrom, time::UNIX_EPOCH};

#[derive(Clone, Deserialize, Serialize, Debug)]
pub struct PduEvent {
    pub event_id: EventId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub origin_server_ts: UInt,
    #[serde(rename = "type")]
    pub kind: EventType,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
    pub prev_events: Vec<EventId>,
    pub depth: UInt,
    pub auth_events: Vec<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacts: Option<EventId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unsigned: BTreeMap<String, serde_json::Value>,
    pub hashes: EventHash,
    pub signatures: BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>,
}

impl PduEvent {
    pub fn redact(&mut self, reason: &PduEvent) -> crate::Result<()> {
        self.unsigned.clear();

        let allowed: &[&str] = match self.kind {
            EventType::RoomMember => &["membership"],
            EventType::RoomCreate => &["creator"],
            EventType::RoomJoinRules => &["join_rule"],
            EventType::RoomPowerLevels => &[
                "ban",
                "events",
                "events_default",
                "kick",
                "redact",
                "state_default",
                "users",
                "users_default",
            ],
            EventType::RoomHistoryVisibility => &["history_visibility"],
            _ => &[],
        };

        let old_content = self
            .content
            .as_object_mut()
            .ok_or_else(|| Error::bad_database("PDU in db has invalid content."))?;

        let mut new_content = serde_json::Map::new();

        for key in allowed {
            if let Some(value) = old_content.remove(*key) {
                new_content.insert((*key).to_owned(), value);
            }
        }

        self.unsigned.insert(
            "redacted_because".to_owned(),
            serde_json::to_string(reason)
                .expect("PduEvent::to_string always works")
                .into(),
        );

        self.content = new_content.into();

        Ok(())
    }

    pub fn to_sync_room_event(&self) -> Raw<AnySyncRoomEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    /// This only works for events that are also AnyRoomEvents.
    pub fn to_any_event(&self) -> Raw<AnyEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    pub fn to_room_event(&self) -> Raw<AnyRoomEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    pub fn to_state_event(&self) -> Raw<AnyStateEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    pub fn to_sync_state_event(&self) -> Raw<AnySyncStateEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    pub fn to_stripped_state_event(&self) -> Raw<AnyStrippedStateEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "sender": self.sender,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    pub fn to_member_event(&self) -> Raw<StateEvent<MemberEventContent>> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "redacts": self.redacts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    /// This does not return a full `Pdu` it is only to satisfy ruma's types.
    pub fn convert_to_outgoing_federation_event(
        mut pdu_json: CanonicalJsonObject,
    ) -> Raw<ruma::events::pdu::Pdu> {
        if let Some(CanonicalJsonValue::Object(unsigned)) = pdu_json.get_mut("unsigned") {
            unsigned.remove("transaction_id");
        }

        pdu_json.remove("event_id");

        // TODO: another option would be to convert it to a canonical string to validate size
        // and return a Result<Raw<...>>
        // serde_json::from_str::<Raw<_>>(
        //     ruma::serde::to_canonical_json_string(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        // )
        // .expect("Raw::from_value always works")

        serde_json::from_value::<Raw<_>>(
            serde_json::to_value(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        )
        .expect("Raw::from_value always works")
    }

    pub fn from_id_val(
        event_id: &EventId,
        mut json: CanonicalJsonObject,
    ) -> Result<Self, serde_json::Error> {
        json.insert(
            "event_id".to_string(),
            ruma::serde::to_canonical_value(event_id).expect("event_id is a valid Value"),
        );

        serde_json::from_value(serde_json::to_value(json).expect("valid JSON"))
    }
}

impl state_res::Event for PduEvent {
    fn event_id(&self) -> &EventId {
        &self.event_id
    }

    fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    fn sender(&self) -> &UserId {
        &self.sender
    }
    fn kind(&self) -> EventType {
        self.kind.clone()
    }

    fn content(&self) -> serde_json::Value {
        self.content.clone()
    }
    fn origin_server_ts(&self) -> std::time::SystemTime {
        UNIX_EPOCH + std::time::Duration::from_millis(self.origin_server_ts.into())
    }

    fn state_key(&self) -> Option<String> {
        self.state_key.clone()
    }
    fn prev_events(&self) -> Vec<EventId> {
        self.prev_events.to_vec()
    }
    fn depth(&self) -> &UInt {
        &self.depth
    }
    fn auth_events(&self) -> Vec<EventId> {
        self.auth_events.to_vec()
    }
    fn redacts(&self) -> Option<&EventId> {
        self.redacts.as_ref()
    }
    fn hashes(&self) -> &EventHash {
        &self.hashes
    }
    fn signatures(&self) -> BTreeMap<Box<ServerName>, BTreeMap<ruma::ServerSigningKeyId, String>> {
        self.signatures.clone()
    }
    fn unsigned(&self) -> &BTreeMap<String, serde_json::Value> {
        &self.unsigned
    }
}

// These impl's allow us to dedup state snapshots when resolving state
// for incoming events (federation/send/{txn}).
impl Eq for PduEvent {}
impl PartialEq for PduEvent {
    fn eq(&self, other: &Self) -> bool {
        self.event_id == other.event_id
    }
}
impl PartialOrd for PduEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.event_id.partial_cmp(&other.event_id)
    }
}
impl Ord for PduEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event_id.cmp(&other.event_id)
    }
}

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU with the eventId inserted as a `serde_json::Value`.
pub(crate) fn process_incoming_pdu(
    pdu: &Raw<ruma::events::pdu::Pdu>,
) -> (EventId, CanonicalJsonObject) {
    let mut value =
        serde_json::from_str(pdu.json().get()).expect("A Raw<...> is always valid JSON");

    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value, &RoomVersionId::Version6)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    value.insert(
        "event_id".to_owned(),
        to_canonical_value(&event_id).expect("EventId is a valid CanonicalJsonValue"),
    );

    (event_id, value)
}

/// Build the start of a PDU in order to add it to the `Database`.
#[derive(Debug, Deserialize)]
pub struct PduBuilder {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub content: serde_json::Value,
    pub unsigned: Option<BTreeMap<String, serde_json::Value>>,
    pub state_key: Option<String>,
    pub redacts: Option<EventId>,
}
