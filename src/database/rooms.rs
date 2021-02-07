mod edus;

pub use edus::RoomEdus;

use crate::{pdu::PduBuilder, utils, Error, PduEvent, Result};
use log::error;
use regex::Regex;
use ring::digest;
use ruma::{
    api::client::error::ErrorKind,
    events::{
        ignored_user_list,
        room::{
            member, message,
            power_levels::{self, PowerLevelsEventContent},
        },
        EventType,
    },
    serde::{to_canonical_value, CanonicalJsonObject, CanonicalJsonValue, Raw},
    EventId, RoomAliasId, RoomId, RoomVersionId, ServerName, UserId,
};
use sled::IVec;
use state_res::{event_auth, Error as StateError, Requester, StateEvent, StateMap, StateStore};

use std::{
    collections::{BTreeMap, HashMap},
    convert::{TryFrom, TryInto},
    mem,
    sync::Arc,
};

use super::admin::AdminCommand;

/// The unique identifier of each state group.
///
/// This is created when a state group is added to the database by
/// hashing the entire state.
pub type StateHashId = IVec;

#[derive(Clone)]
pub struct Rooms {
    pub edus: edus::RoomEdus,
    pub(super) pduid_pdu: sled::Tree, // PduId = RoomId + Count
    pub(super) eventid_pduid: sled::Tree,
    pub(super) roomid_pduleaves: sled::Tree,
    pub(super) alias_roomid: sled::Tree,
    pub(super) aliasid_alias: sled::Tree, // AliasId = RoomId + Count
    pub(super) publicroomids: sled::Tree,

    pub(super) tokenids: sled::Tree, // TokenId = RoomId + Token + PduId

    /// Participating servers in a room.
    pub(super) roomserverids: sled::Tree, // RoomServerId = RoomId + ServerName
    pub(super) userroomid_joined: sled::Tree,
    pub(super) roomuserid_joined: sled::Tree,
    pub(super) roomuseroncejoinedids: sled::Tree,
    pub(super) userroomid_invited: sled::Tree,
    pub(super) roomuserid_invited: sled::Tree,
    pub(super) userroomid_left: sled::Tree,

    /// Remember the current state hash of a room.
    pub(super) roomid_statehash: sled::Tree,
    /// Remember the state hash at events in the past.
    pub(super) pduid_statehash: sled::Tree,
    /// The state for a given state hash.
    pub(super) statekey_short: sled::Tree, // StateKey = EventType + StateKey, Short = Count
    pub(super) stateid_pduid: sled::Tree, // StateId = StateHash + Short, PduId = Count (without roomid)
}

impl StateStore for Rooms {
    fn get_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> state_res::Result<Arc<StateEvent>> {
        let pid = self
            .get_pdu_id(event_id)
            .map_err(StateError::custom)?
            .ok_or_else(|| {
                StateError::NotFound(format!(
                    "PDU via room_id and event_id not found in the db: {}",
                    event_id.as_str()
                ))
            })?;

        serde_json::from_slice(
            &self
                .pduid_pdu
                .get(pid)
                .map_err(StateError::custom)?
                .ok_or_else(|| StateError::NotFound("PDU via pduid not found in db.".into()))?,
        )
        .map_err(Into::into)
        .and_then(|pdu: StateEvent| {
            // conduit's PDU's always contain a room_id but some
            // of ruma's do not so this must be an Option
            if pdu.room_id() == room_id {
                Ok(Arc::new(pdu))
            } else {
                Err(StateError::NotFound(
                    "Found PDU for incorrect room in db.".into(),
                ))
            }
        })
    }
}

impl Rooms {
    /// Builds a StateMap by iterating over all keys that start
    /// with state_hash, this gives the full state for the given state_hash.
    pub fn state_full(
        &self,
        room_id: &RoomId,
        state_hash: &StateHashId,
    ) -> Result<StateMap<PduEvent>> {
        self.stateid_pduid
            .scan_prefix(&state_hash)
            .values()
            .map(|pduid_short| {
                let mut pduid = room_id.as_bytes().to_vec();
                pduid.push(0xff);
                pduid.extend_from_slice(&pduid_short?);
                self.pduid_pdu.get(&pduid)?.map_or_else(
                    || Err(Error::bad_database("Failed to find PDU in state snapshot.")),
                    |b| {
                        serde_json::from_slice::<PduEvent>(&b)
                            .map_err(|_| Error::bad_database("Invalid PDU in db."))
                    },
                )
            })
            .filter_map(|r| r.ok())
            .map(|pdu| {
                Ok((
                    (
                        pdu.kind.clone(),
                        pdu.state_key
                            .as_ref()
                            .ok_or_else(|| Error::bad_database("State event has no state key."))?
                            .clone(),
                    ),
                    pdu,
                ))
            })
            .collect::<Result<StateMap<_>>>()
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    pub fn state_get(
        &self,
        room_id: &RoomId,
        state_hash: &StateHashId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<(IVec, PduEvent)>> {
        let mut key = event_type.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&state_key.as_bytes());

        let short = self.statekey_short.get(&key)?;

        if let Some(short) = short {
            let mut stateid = state_hash.to_vec();
            stateid.push(0xff);
            stateid.extend_from_slice(&short);

            self.stateid_pduid
                .get(&stateid)?
                .map_or(Ok(None), |pdu_id_short| {
                    let mut pdu_id = room_id.as_bytes().to_vec();
                    pdu_id.push(0xff);
                    pdu_id.extend_from_slice(&pdu_id_short);

                    Ok::<_, Error>(Some((
                        pdu_id.clone().into(),
                        serde_json::from_slice::<PduEvent>(
                            &self.pduid_pdu.get(&pdu_id)?.ok_or_else(|| {
                                Error::bad_database("PDU in state not found in database.")
                            })?,
                        )
                        .map_err(|_| Error::bad_database("Invalid PDU bytes in room state."))?,
                    )))
                })
        } else {
            return Ok(None);
        }
    }

    /// Returns the last state hash key added to the db.
    pub fn pdu_state_hash(&self, pdu_id: &[u8]) -> Result<Option<StateHashId>> {
        Ok(self.pduid_statehash.get(pdu_id)?)
    }

    /// Returns the last state hash key added to the db for the given room.
    pub fn current_state_hash(&self, room_id: &RoomId) -> Result<Option<StateHashId>> {
        Ok(self.roomid_statehash.get(room_id.as_bytes())?)
    }

    /// This fetches auth events from the current state.
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &EventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: serde_json::Value,
    ) -> Result<StateMap<PduEvent>> {
        let auth_events = state_res::auth_types_for_event(
            kind.clone(),
            sender,
            state_key.map(|s| s.to_string()),
            content,
        );

        let mut events = StateMap::new();
        for (event_type, state_key) in auth_events {
            if let Some((_, pdu)) = self.room_state_get(room_id, &event_type, &state_key)? {
                events.insert((event_type, state_key), pdu);
            }
        }
        Ok(events)
    }

    /// Generate a new StateHash.
    ///
    /// A unique hash made from hashing all PDU ids of the state joined with 0xff.
    fn calculate_hash(&self, pdu_id_bytes: &[&[u8]]) -> Result<StateHashId> {
        // We only hash the pdu's event ids, not the whole pdu
        let bytes = pdu_id_bytes.join(&0xff);
        let hash = digest::digest(&digest::SHA256, &bytes);
        Ok(hash.as_ref().into())
    }

    /// Checks if a room exists.
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .get_gt(&prefix)?
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    /// Force the creation of a new StateHash and insert it into the db.
    ///
    /// Whatever `state` is supplied to `force_state` __is__ the current room state snapshot.
    pub fn force_state(
        &self,
        room_id: &RoomId,
        state: HashMap<(EventType, String), Vec<u8>>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let state_hash =
            self.calculate_hash(&state.values().map(|pdu_id| &**pdu_id).collect::<Vec<_>>())?;
        let mut prefix = state_hash.to_vec();
        prefix.push(0xff);

        for ((event_type, state_key), pdu_id) in state {
            let mut statekey = event_type.as_ref().as_bytes().to_vec();
            statekey.push(0xff);
            statekey.extend_from_slice(&state_key.as_bytes());

            let short = match self.statekey_short.get(&statekey)? {
                Some(short) => utils::u64_from_bytes(&short)
                    .map_err(|_| Error::bad_database("Invalid short bytes in statekey_short."))?,
                None => {
                    let short = globals.next_count()?;
                    self.statekey_short
                        .insert(&statekey, &short.to_be_bytes())?;
                    short
                }
            };

            let pdu_id_short = pdu_id
                .splitn(2, |&b| b == 0xff)
                .nth(1)
                .ok_or_else(|| Error::bad_database("Invalid pduid in state."))?;

            let mut state_id = prefix.clone();
            state_id.extend_from_slice(&short.to_be_bytes());
            self.stateid_pduid.insert(state_id, pdu_id_short)?;
        }

        self.roomid_statehash
            .insert(room_id.as_bytes(), &*state_hash)?;

        Ok(())
    }

    /// Returns the full room state.
    pub fn room_state_full(&self, room_id: &RoomId) -> Result<StateMap<PduEvent>> {
        if let Some(current_state_hash) = self.current_state_hash(room_id)? {
            self.state_full(&room_id, &current_state_hash)
        } else {
            Ok(BTreeMap::new())
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<(IVec, PduEvent)>> {
        if let Some(current_state_hash) = self.current_state_hash(room_id)? {
            self.state_get(&room_id, &current_state_hash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns the `count` of this pdu's id.
    pub fn pdu_count(&self, pdu_id: &[u8]) -> Result<u64> {
        Ok(
            utils::u64_from_bytes(&pdu_id[pdu_id.len() - mem::size_of::<u64>()..pdu_id.len()])
                .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?,
        )
    }

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| self.pdu_count(&pdu_id).map(Some))
    }

    /// Returns the json of a pdu.
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<serde_json::Value>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }

    /// Returns the pdu's id.
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<IVec>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| Ok(Some(pdu_id)))
    }

    /// Returns the pdu.
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }
    /// Returns the pdu.
    pub fn get_pdu_from_id(&self, pdu_id: &IVec) -> Result<Option<PduEvent>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    pub fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(&self, pdu_id: &IVec, pdu: &PduEvent) -> Result<()> {
        if self.pduid_pdu.get(&pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                &pdu_id,
                &*serde_json::to_string(pdu).expect("PduEvent::to_string always works"),
            )?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "PDU does not exist.",
            ))
        }
    }

    /// Returns the leaf pdus of a room.
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<Vec<EventId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut events = Vec::new();

        for event in self
            .roomid_pduleaves
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok::<_, Error>(
                    EventId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                        Error::bad_database("EventID in roomid_pduleaves is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("EventId in roomid_pduleaves is invalid."))?,
                )
            })
        {
            events.push(event?);
        }

        Ok(events)
    }

    /// Replace the leaves of a room with a new event.
    pub fn replace_pdu_leaves(&self, room_id: &RoomId, event_id: &EventId) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        for key in self.roomid_pduleaves.scan_prefix(&prefix).keys() {
            self.roomid_pduleaves.remove(key?)?;
        }

        prefix.extend_from_slice(event_id.as_bytes());
        self.roomid_pduleaves.insert(&prefix, event_id.as_bytes())?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    ///
    /// By this point the incoming event should be fully authenticated, no auth happens
    /// in `append_pdu`.
    pub fn append_pdu(
        &self,
        pdu: &PduEvent,
        mut pdu_json: CanonicalJsonObject,
        count: u64,
        pdu_id: IVec,
        globals: &super::globals::Globals,
        account_data: &super::account_data::AccountData,
        admin: &super::admin::Admin,
    ) -> Result<()> {
        // Make unsigned fields correct. This is not properly documented in the spec, but state
        // events need to have previous content in the unsigned field, so clients can easily
        // interpret things like membership changes
        if let Some(state_key) = &pdu.state_key {
            if let CanonicalJsonValue::Object(unsigned) = pdu_json
                .entry("unsigned".to_owned())
                .or_insert_with(|| CanonicalJsonValue::Object(Default::default()))
            {
                if let Some(prev_state_hash) = self.pdu_state_hash(&pdu_id).unwrap() {
                    if let Some(prev_state) = self
                        .state_get(&pdu.room_id, &prev_state_hash, &pdu.kind, &state_key)
                        .unwrap()
                    {
                        unsigned.insert(
                            "prev_content".to_owned(),
                            CanonicalJsonValue::Object(
                                utils::to_canonical_object(prev_state.1.content)
                                    .expect("event is valid, we just created it"),
                            ),
                        );
                    }
                }
            } else {
                error!("Invalid unsigned type in pdu.");
            }
        }

        self.replace_pdu_leaves(&pdu.room_id, &pdu.event_id)?;

        // Mark as read first so the sending client doesn't get a notification even if appending
        // fails
        self.edus
            .private_read_set(&pdu.room_id, &pdu.sender, count, &globals)?;

        self.pduid_pdu.insert(
            &pdu_id,
            &*serde_json::to_string(&pdu_json)
                .expect("CanonicalJsonObject is always a valid String"),
        )?;

        self.eventid_pduid
            .insert(pdu.event_id.as_bytes(), &*pdu_id)?;

        // See if the event matches any known pushers
        db.sending.send_push_pdu(&*pdu_id)?;

        match pdu.kind {
            EventType::RoomRedaction => {
                if let Some(redact_id) = &pdu.redacts {
                    self.redact_pdu(&redact_id, &pdu)?;
                }
            }
            EventType::RoomMember => {
                if let Some(state_key) = &pdu.state_key {
                    // if the state_key fails
                    let target_user_id = UserId::try_from(state_key.clone())
                        .expect("This state_key was previously validated");
                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    self.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        serde_json::from_value::<member::MemberEventContent>(pdu.content.clone())
                            .map_err(|_| {
                            Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Invalid member event content.",
                            )
                        })?,
                        &pdu.sender,
                        account_data,
                        globals,
                    )?;
                }
            }
            EventType::RoomMessage => {
                if let Some(body) = pdu.content.get("body").and_then(|b| b.as_str()) {
                    for word in body
                        .split_terminator(|c: char| !c.is_alphanumeric())
                        .map(str::to_lowercase)
                    {
                        let mut key = pdu.room_id.to_string().as_bytes().to_vec();
                        key.push(0xff);
                        key.extend_from_slice(word.as_bytes());
                        key.push(0xff);
                        key.extend_from_slice(&pdu_id);
                        self.tokenids.insert(key, &[])?;
                    }

                    if body.starts_with(&format!("@conduit:{}: ", globals.server_name()))
                        && self
                            .id_from_alias(
                                &format!("#admins:{}", globals.server_name())
                                    .try_into()
                                    .expect("#admins:server_name is a valid room alias"),
                            )?
                            .as_ref()
                            == Some(&pdu.room_id)
                    {
                        let mut lines = body.lines();
                        let command_line = lines.next().expect("each string has at least one line");
                        let body = lines.collect::<Vec<_>>();

                        let mut parts = command_line.split_whitespace().skip(1);
                        if let Some(command) = parts.next() {
                            let args = parts.collect::<Vec<_>>();

                            match command {
                                "register_appservice" => {
                                    if body.len() > 2
                                        && body[0].trim() == "```"
                                        && body.last().unwrap().trim() == "```"
                                    {
                                        let appservice_config = body[1..body.len() - 1].join("\n");
                                        let parsed_config = serde_yaml::from_str::<serde_yaml::Value>(
                                            &appservice_config,
                                        );
                                        match parsed_config {
                                            Ok(yaml) => {
                                                admin.send(AdminCommand::RegisterAppservice(yaml));
                                            }
                                            Err(e) => {
                                                admin.send(AdminCommand::SendMessage(
                                                    message::MessageEventContent::text_plain(
                                                        format!(
                                                            "Could not parse appservice config: {}",
                                                            e
                                                        ),
                                                    ),
                                                ));
                                            }
                                        }
                                    } else {
                                        admin.send(AdminCommand::SendMessage(
                                            message::MessageEventContent::text_plain(
                                                "Expected code block in command body.",
                                            ),
                                        ));
                                    }
                                }
                                "list_appservices" => {
                                    admin.send(AdminCommand::ListAppservices);
                                }
                                _ => {
                                    admin.send(AdminCommand::SendMessage(
                                        message::MessageEventContent::text_plain(format!(
                                            "Command: {}, Args: {:?}",
                                            command, args
                                        )),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `pduid_statehash`.
    /// The incoming event is the `pdu_id` passed to this method.
    pub fn append_to_state(
        &self,
        new_pdu_id: &[u8],
        new_pdu: &PduEvent,
        globals: &super::globals::Globals,
    ) -> Result<StateHashId> {
        let old_state =
            if let Some(old_state_hash) = self.roomid_statehash.get(new_pdu.room_id.as_bytes())? {
                // Store state for event. The state does not include the event itself.
                // Instead it's the state before the pdu, so the room's old state.
                self.pduid_statehash.insert(new_pdu_id, &old_state_hash)?;
                if new_pdu.state_key.is_none() {
                    return Ok(old_state_hash);
                }

                let mut prefix = old_state_hash.to_vec();
                prefix.push(0xff);
                self.stateid_pduid
                    .scan_prefix(&prefix)
                    .filter_map(|pdu| pdu.map_err(|e| error!("{}", e)).ok())
                    // Chop the old state_hash out leaving behind the short key (u64)
                    .map(|(k, v)| (k.subslice(prefix.len(), k.len() - prefix.len()), v))
                    .collect::<HashMap<IVec, IVec>>()
            } else {
                HashMap::new()
            };

        if let Some(state_key) = &new_pdu.state_key {
            let mut new_state = old_state;
            let mut pdu_key = new_pdu.kind.as_ref().as_bytes().to_vec();
            pdu_key.push(0xff);
            pdu_key.extend_from_slice(state_key.as_bytes());

            let short = match self.statekey_short.get(&pdu_key)? {
                Some(short) => utils::u64_from_bytes(&short)
                    .map_err(|_| Error::bad_database("Invalid short bytes in statekey_short."))?,
                None => {
                    let short = globals.next_count()?;
                    self.statekey_short.insert(&pdu_key, &short.to_be_bytes())?;
                    short
                }
            };

            let new_pdu_id_short = new_pdu_id
                .splitn(2, |&b| b == 0xff)
                .nth(1)
                .ok_or_else(|| Error::bad_database("Invalid pduid in state."))?;

            new_state.insert((&short.to_be_bytes()).into(), new_pdu_id_short.into());

            let new_state_hash =
                self.calculate_hash(&new_state.values().map(|b| &**b).collect::<Vec<_>>())?;

            let mut key = new_state_hash.to_vec();
            key.push(0xff);

            for (short, short_pdu_id) in new_state {
                let mut state_id = key.clone();
                state_id.extend_from_slice(&short);
                self.stateid_pduid.insert(&state_id, &short_pdu_id)?;
            }

            Ok(new_state_hash)
        } else {
            Err(Error::bad_database(
                "Tried to insert non-state event into room without a state.",
            ))
        }
    }

    pub fn set_room_state(&self, room_id: &RoomId, state_hash: &StateHashId) -> Result<()> {
        self.roomid_statehash
            .insert(room_id.as_bytes(), state_hash)?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    #[allow(clippy::too_many_arguments)]
    pub fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        globals: &super::globals::Globals,
        sending: &super::sending::Sending,
        admin: &super::admin::Admin,
        account_data: &super::account_data::AccountData,
        appservice: &super::appservice::Appservice,
    ) -> Result<EventId> {
        let PduBuilder {
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
        } = pdu_builder;
        // TODO: Make sure this isn't called twice in parallel
        let prev_events = self.get_pdu_leaves(&room_id)?;

        let auth_events = self.get_auth_events(
            &room_id,
            &event_type,
            &sender,
            state_key.as_deref(),
            content.clone(),
        )?;

        // Is the event authorized?
        if let Some(state_key) = &state_key {
            let power_levels = self
                .room_state_get(&room_id, &EventType::RoomPowerLevels, "")?
                .map_or_else(
                    || {
                        Ok::<_, Error>(power_levels::PowerLevelsEventContent {
                            ban: 50.into(),
                            events: BTreeMap::new(),
                            events_default: 0.into(),
                            invite: 50.into(),
                            kick: 50.into(),
                            redact: 50.into(),
                            state_default: 0.into(),
                            users: BTreeMap::new(),
                            users_default: 0.into(),
                            notifications:
                                ruma::events::room::power_levels::NotificationPowerLevels {
                                    room: 50.into(),
                                },
                        })
                    },
                    |(_, power_levels)| {
                        Ok(serde_json::from_value::<Raw<PowerLevelsEventContent>>(
                            power_levels.content,
                        )
                        .expect("Raw::from_value always works.")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid PowerLevels event in db."))?)
                    },
                )?;
            let sender_membership = self
                .room_state_get(&room_id, &EventType::RoomMember, &sender.to_string())?
                .map_or(
                    Ok::<_, Error>(member::MembershipState::Leave),
                    |(_, pdu)| {
                        Ok(
                            serde_json::from_value::<Raw<member::MemberEventContent>>(pdu.content)
                                .expect("Raw::from_value always works.")
                                .deserialize()
                                .map_err(|_| Error::bad_database("Invalid Member event in db."))?
                                .membership,
                        )
                    },
                )?;

            let sender_power = power_levels.users.get(&sender).map_or_else(
                || {
                    if sender_membership != member::MembershipState::Join {
                        None
                    } else {
                        Some(&power_levels.users_default)
                    }
                },
                // If it's okay, wrap with Some(_)
                Some,
            );

            // Is the event allowed?
            #[allow(clippy::blocks_in_if_conditions)]
            if !match event_type {
                EventType::RoomEncryption => {
                    // Only allow encryption events if it's allowed in the config
                    globals.allow_encryption()
                }
                EventType::RoomMember => {
                    let prev_event = self
                        .get_pdu(prev_events.get(0).ok_or(Error::BadRequest(
                            ErrorKind::Unknown,
                            "Membership can't be the first event",
                        ))?)?
                        .map(|pdu| pdu.convert_for_state_res());
                    event_auth::valid_membership_change(
                        // TODO this is a bit of a hack but not sure how to have a type
                        // declared in `state_res` crate easily convert to/from conduit::PduEvent
                        Requester {
                            prev_event_ids: prev_events.to_owned(),
                            room_id: &room_id,
                            content: &content,
                            state_key: Some(state_key.to_owned()),
                            sender: &sender,
                        },
                        prev_event,
                        None, // TODO: third party invite
                        &auth_events
                            .iter()
                            .map(|((ty, key), pdu)| {
                                Ok(((ty.clone(), key.clone()), pdu.convert_for_state_res()))
                            })
                            .collect::<Result<StateMap<_>>>()?,
                    )
                    .map_err(|e| {
                        log::error!("{}", e);
                        Error::Conflict("Found incoming PDU with invalid data.")
                    })?
                }
                EventType::RoomCreate => prev_events.is_empty(),
                // Not allow any of the following events if the sender is not joined.
                _ if sender_membership != member::MembershipState::Join => false,
                _ => {
                    // TODO
                    sender_power.unwrap_or(&power_levels.users_default)
                        >= &power_levels.state_default
                }
            } {
                error!("Unauthorized {}", event_type);
                // Not authorized
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Event is not authorized",
                ));
            }
        } else if !self.is_joined(&sender, &room_id)? {
            // TODO: auth rules apply to all events, not only those with a state key
            error!("Unauthorized {}", event_type);
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Event is not authorized",
            ));
        }

        // Our depth is the maximum depth of prev_events + 1
        let depth = prev_events
            .iter()
            .filter_map(|event_id| Some(self.get_pdu_json(event_id).ok()??.get("depth")?.as_u64()?))
            .max()
            .unwrap_or(0_u64)
            + 1;

        let mut unsigned = unsigned.unwrap_or_default();
        if let Some(state_key) = &state_key {
            if let Some((_, prev_pdu)) = self.room_state_get(&room_id, &event_type, &state_key)? {
                unsigned.insert("prev_content".to_owned(), prev_pdu.content);
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(prev_pdu.sender).expect("UserId::to_value always works"),
                );
            }
        }

        let mut pdu = PduEvent {
            event_id: ruma::event_id!("$thiswillbefilledinlater"),
            room_id: room_id.clone(),
            sender: sender.clone(),
            origin_server_ts: utils::millis_since_unix_epoch()
                .try_into()
                .expect("time is valid"),
            kind: event_type,
            content,
            state_key,
            prev_events,
            depth: depth
                .try_into()
                .map_err(|_| Error::bad_database("Depth is invalid"))?,
            auth_events: auth_events
                .into_iter()
                .map(|(_, pdu)| pdu.event_id)
                .collect(),
            redacts,
            unsigned,
            hashes: ruma::events::pdu::EventHash {
                sha256: "aaa".to_owned(),
            },
            signatures: BTreeMap::new(),
        };

        // Hash and sign
        let mut pdu_json =
            utils::to_canonical_object(&pdu).expect("event is valid, we just created it");

        pdu_json.remove("event_id");

        // Add origin because synapse likes that (and it's required in the spec)
        pdu_json.insert(
            "origin".to_owned(),
            to_canonical_value(globals.server_name())
                .expect("server name is a valid CanonicalJsonValue"),
        );

        ruma::signatures::hash_and_sign_event(
            globals.server_name().as_str(),
            globals.keypair(),
            &mut pdu_json,
            &RoomVersionId::Version6,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        pdu.event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&pdu_json, &RoomVersionId::Version6)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        pdu_json.insert(
            "event_id".to_owned(),
            to_canonical_value(&pdu.event_id).expect("EventId is a valid CanonicalJsonValue"),
        );

        // Increment the last index and use that
        // This is also the next_batch/since value
        let count = globals.next_count()?;
        let mut pdu_id = room_id.as_bytes().to_vec();
        pdu_id.push(0xff);
        pdu_id.extend_from_slice(&count.to_be_bytes());

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = self.append_to_state(&pdu_id, &pdu, &globals)?;

        self.append_pdu(
            &pdu,
            pdu_json,
            count,
            pdu_id.clone().into(),
            globals,
            account_data,
            admin,
        )?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        self.set_room_state(&room_id, &statehashid)?;

        for server in self
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .filter(|server| &**server != globals.server_name())
        {
            sending.send_pdu(&server, &pdu_id)?;
        }

        for appservice in appservice.iter_all().filter_map(|r| r.ok()) {
            if let Some(namespaces) = appservice.1.get("namespaces") {
                let users = namespaces
                    .get("users")
                    .and_then(|users| users.as_sequence())
                    .map_or_else(
                        || Vec::new(),
                        |users| {
                            users
                                .iter()
                                .map(|users| {
                                    users
                                        .get("regex")
                                        .and_then(|regex| regex.as_str())
                                        .and_then(|regex| Regex::new(regex).ok())
                                })
                                .filter_map(|o| o)
                                .collect::<Vec<_>>()
                        },
                    );
                let aliases = namespaces
                    .get("aliases")
                    .and_then(|users| users.get("regex"))
                    .and_then(|regex| regex.as_str())
                    .and_then(|regex| Regex::new(regex).ok());
                let rooms = namespaces
                    .get("rooms")
                    .and_then(|rooms| rooms.as_sequence());

                let room_aliases = self.room_aliases(&room_id);

                let bridge_user_id = appservice
                    .1
                    .get("sender_localpart")
                    .and_then(|string| string.as_str())
                    .and_then(|string| {
                        UserId::parse_with_server_name(string, globals.server_name()).ok()
                    });

                if bridge_user_id.map_or(false, |bridge_user_id| {
                    self.is_joined(&bridge_user_id, room_id).unwrap_or(false)
                }) || users.iter().any(|users| {
                    users.is_match(pdu.sender.as_str())
                        || pdu.kind == EventType::RoomMember
                            && pdu
                                .state_key
                                .as_ref()
                                .map_or(false, |state_key| users.is_match(&state_key))
                }) || aliases.map_or(false, |aliases| {
                    room_aliases
                        .filter_map(|r| r.ok())
                        .any(|room_alias| aliases.is_match(room_alias.as_str()))
                }) || rooms.map_or(false, |rooms| rooms.contains(&room_id.as_str().into()))
                    || self
                        .room_members(&room_id)
                        .filter_map(|r| r.ok())
                        .any(|member| users.iter().any(|regex| regex.is_match(member.as_str())))
                {
                    sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
                }
            }
        }

        Ok(pdu.event_id)
    }

    /// Returns an iterator over all PDUs in a room.
    pub fn all_pdus(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<(IVec, PduEvent)>>> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns a double-ended iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    pub fn pdus_since(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl DoubleEndedIterator<Item = Result<(IVec, PduEvent)>>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        // Skip the first pdu if it's exactly at since, because we sent that last time
        let mut first_pdu_id = prefix.clone();
        first_pdu_id.extend_from_slice(&(since + 1).to_be_bytes());

        let mut last_pdu_id = prefix;
        last_pdu_id.extend_from_slice(&u64::MAX.to_be_bytes());

        let user_id = user_id.clone();
        Ok(self
            .pduid_pdu
            .range(first_pdu_id..last_pdu_id)
            .filter_map(|r| r.ok())
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            }))
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    pub fn pdus_until(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> impl Iterator<Item = Result<(IVec, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&until.to_be_bytes());

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        self.pduid_pdu
            .range(..current)
            .rev()
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            })
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    pub fn pdus_after(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> impl Iterator<Item = Result<(IVec, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        self.pduid_pdu
            .range(current..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            })
    }

    /// Replace a PDU with the redacted form.
    pub fn redact_pdu(&self, event_id: &EventId, reason: &PduEvent) -> Result<()> {
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;
            pdu.redact(&reason)?;
            self.replace_pdu(&pdu_id, &pdu)?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Event ID does not exist.",
            ))
        }
    }

    /// Update current membership data.
    fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        member_content: member::MemberEventContent,
        sender: &UserId,
        account_data: &super::account_data::AccountData,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let membership = member_content.membership;

        let mut roomserver_id = room_id.as_bytes().to_vec();
        roomserver_id.push(0xff);
        roomserver_id.extend_from_slice(user_id.server_name().as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        match &membership {
            member::MembershipState::Join => {
                // Check if the user never joined this room
                if !self.once_joined(&user_id, &room_id)? {
                    // Add the user ID to the join list then
                    self.roomuseroncejoinedids.insert(&userroom_id, &[])?;

                    // Check if the room has a predecessor
                    if let Some(predecessor) = self
                        .room_state_get(&room_id, &EventType::RoomCreate, "")?
                        .and_then(|(_, create)| {
                            serde_json::from_value::<
                                Raw<ruma::events::room::create::CreateEventContent>,
                            >(create.content)
                            .expect("Raw::from_value always works")
                            .deserialize()
                            .ok()
                        })
                        .and_then(|content| content.predecessor)
                    {
                        // Copy user settings from predecessor to the current room:
                        // - Push rules
                        //
                        // TODO: finish this once push rules are implemented.
                        //
                        // let mut push_rules_event_content = account_data
                        //     .get::<ruma::events::push_rules::PushRulesEvent>(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //     )?;
                        //
                        // NOTE: find where `predecessor.room_id` match
                        //       and update to `room_id`.
                        //
                        // account_data
                        //     .update(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //         &push_rules_event_content,
                        //         globals,
                        //     )
                        //     .ok();

                        // Copy old tags to new room
                        if let Some(tag_event) = account_data.get::<ruma::events::tag::TagEvent>(
                            Some(&predecessor.room_id),
                            user_id,
                            EventType::Tag,
                        )? {
                            account_data
                                .update(Some(room_id), user_id, EventType::Tag, &tag_event, globals)
                                .ok();
                        };

                        // Copy direct chat flag
                        if let Some(mut direct_event) = account_data
                            .get::<ruma::events::direct::DirectEvent>(
                            None,
                            user_id,
                            EventType::Direct,
                        )? {
                            let mut room_ids_updated = false;

                            for room_ids in direct_event.content.0.values_mut() {
                                if room_ids.iter().any(|r| r == &predecessor.room_id) {
                                    room_ids.push(room_id.clone());
                                    room_ids_updated = true;
                                }
                            }

                            if room_ids_updated {
                                account_data.update(
                                    None,
                                    user_id,
                                    EventType::Direct,
                                    &direct_event,
                                    globals,
                                )?;
                            }
                        };
                    }
                }

                self.roomserverids.insert(&roomserver_id, &[])?;
                self.userroomid_joined.insert(&userroom_id, &[])?;
                self.roomuserid_joined.insert(&roomuser_id, &[])?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Invite => {
                // We want to know if the sender is ignored by the receiver
                let is_ignored = account_data
                    .get::<ignored_user_list::IgnoredUserListEvent>(
                        None,     // Ignored users are in global account data
                        &user_id, // Receiver
                        EventType::IgnoredUserList,
                    )?
                    .map_or(false, |ignored| {
                        ignored.content.ignored_users.contains(&sender)
                    });

                if is_ignored {
                    return Ok(());
                }

                self.roomserverids.insert(&roomserver_id, &[])?;
                self.userroomid_invited.insert(&userroom_id, &[])?;
                self.roomuserid_invited.insert(&roomuser_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Leave | member::MembershipState::Ban => {
                if self
                    .room_members(room_id)
                    .chain(self.room_members_invited(room_id))
                    .filter_map(|r| r.ok())
                    .all(|u| u.server_name() != user_id.server_name())
                {
                    self.roomserverids.remove(&roomserver_id)?;
                }
                self.userroomid_left.insert(&userroom_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Makes a user forget a room.
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_left.remove(userroom_id)?;

        Ok(())
    }

    pub fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        if let Some(room_id) = room_id {
            // New alias
            self.alias_roomid
                .insert(alias.alias(), room_id.as_bytes())?;
            let mut aliasid = room_id.as_bytes().to_vec();
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(aliasid, &*alias.alias())?;
        } else {
            // room_id=None means remove alias
            let room_id = self
                .alias_roomid
                .remove(alias.alias())?
                .ok_or(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ))?;

            for key in self.aliasid_alias.scan_prefix(room_id).keys() {
                self.aliasid_alias.remove(key?)?;
            }
        }

        Ok(())
    }

    pub fn id_from_alias(&self, alias: &RoomAliasId) -> Result<Option<RoomId>> {
        self.alias_roomid
            .get(alias.alias())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(
                    RoomId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("Room ID in alias_roomid is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("Room ID in alias_roomid is invalid."))?,
                ))
            })
    }

    pub fn room_aliases(&self, room_id: &RoomId) -> impl Iterator<Item = Result<RoomAliasId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok(serde_json::from_slice(&bytes?)
                    .map_err(|_| Error::bad_database("Alias in aliasid_alias is invalid."))?)
            })
    }

    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.as_bytes(), &[])?;
        } else {
            self.publicroomids.remove(room_id.as_bytes())?;
        }

        Ok(())
    }

    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.contains_key(room_id.as_bytes())?)
    }

    pub fn public_rooms(&self) -> impl Iterator<Item = Result<RoomId>> {
        self.publicroomids.iter().keys().map(|bytes| {
            Ok(
                RoomId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))?,
            )
        })
    }

    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<(impl Iterator<Item = IVec> + 'a, Vec<String>)> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let words = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();

        let iterators = words.clone().into_iter().map(move |word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);
            self.tokenids
                .scan_prefix(&prefix2)
                .keys()
                .rev() // Newest pdus first
                .filter_map(|r| r.ok())
                .map(|key| {
                    let pduid_index = key
                        .iter()
                        .enumerate()
                        .filter(|(_, &b)| b == 0xff)
                        .nth(1)
                        .ok_or_else(|| Error::bad_database("Invalid tokenid in db."))?
                        .0
                        + 1; // +1 because the pdu id starts AFTER the separator

                    let pdu_id = key.subslice(pduid_index, key.len() - pduid_index);

                    Ok::<_, Error>(pdu_id)
                })
                .filter_map(|r| r.ok())
        });

        Ok((
            utils::common_elements(iterators, |a, b| {
                // We compare b with a because we reversed the iterator earlier
                b.cmp(a)
            })
            .unwrap(),
            words,
        ))
    }

    pub fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<UserId>,
    ) -> impl Iterator<Item = Result<RoomId>> + 'a {
        let iterators = users.into_iter().map(move |user_id| {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);

            self.userroomid_joined
                .scan_prefix(&prefix)
                .keys()
                .filter_map(|r| r.ok())
                .map(|key| {
                    let roomid_index = key
                        .iter()
                        .enumerate()
                        .find(|(_, &b)| b == 0xff)
                        .ok_or_else(|| Error::bad_database("Invalid userroomid_joined in db."))?
                        .0
                        + 1; // +1 because the room id starts AFTER the separator

                    let room_id = key.subslice(roomid_index, key.len() - roomid_index);

                    Ok::<_, Error>(room_id)
                })
                .filter_map(|r| r.ok())
        });

        // We use the default compare function because keys are sorted correctly (not reversed)
        utils::common_elements(iterators, Ord::cmp)
            .expect("users is not empty")
            .map(|bytes| {
                RoomId::try_from(utils::string_from_bytes(&*bytes).map_err(|_| {
                    Error::bad_database("Invalid RoomId bytes in userroomid_joined")
                })?)
                .map_err(|_| Error::bad_database("Invalid RoomId in userroomid_joined."))
            })
    }

    /// Returns an iterator over all joined members of a room.
    pub fn room_servers(&self, room_id: &RoomId) -> impl Iterator<Item = Result<Box<ServerName>>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomserverids.scan_prefix(prefix).keys().map(|key| {
            Ok(Box::<ServerName>::try_from(
                utils::string_from_bytes(
                    &key?
                        .rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Server name in roomserverids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Server name in roomserverids is invalid."))?)
        })
    }

    /// Returns an iterator over all joined members of a room.
    pub fn room_members(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_joined
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    pub fn room_useroncejoined(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuseroncejoinedids
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in room_useroncejoined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in room_useroncejoined is invalid."))?)
            })
    }

    /// Returns an iterator over all invited members of a room.
    pub fn room_members_invited(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_invited
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_invited is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms this user joined.
    pub fn rooms_joined(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_joined
            .scan_prefix(user_id.as_bytes())
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user was invited to.
    pub fn rooms_invited(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_invited
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user left.
    pub fn rooms_left(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_left.scan_prefix(prefix).keys().map(|key| {
            Ok(RoomId::try_from(
                utils::string_from_bytes(
                    &key?
                        .rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Room ID in userroomid_left is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in userroomid_left is invalid."))?)
        })
    }

    pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.roomuseroncejoinedids.get(userroom_id)?.is_some())
    }

    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_joined.get(userroom_id)?.is_some())
    }

    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_invited.get(userroom_id)?.is_some())
    }

    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_left.get(userroom_id)?.is_some())
    }
}
