use super::State;
use crate::{pdu::PduBuilder, utils, ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::profile::{
            get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
        },
    },
    events::EventType,
    serde::Raw,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};
use std::convert::TryInto;

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/profile/<_>/displayname", data = "<body>")
)]
pub async fn set_displayname_route(
    db: State<'_, Database>,
    body: Ruma<set_display_name::Request<'_>>,
) -> ConduitResult<set_display_name::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.users
        .set_displayname(&sender_user, body.displayname.clone())?;

    // Send a new membership event and presence update into all joined rooms
    for room_id in db.rooms.rooms_joined(&sender_user) {
        let room_id = room_id?;
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(ruma::events::room::member::MemberEventContent {
                    displayname: body.displayname.clone(),
                    ..serde_json::from_value::<Raw<_>>(
                        db.rooms
                            .room_state_get(
                                &room_id,
                                &EventType::RoomMember,
                                &sender_user.to_string(),
                            )?
                            .ok_or_else(|| {
                                Error::bad_database(
                                    "Tried to send displayname update for user not in the room.",
                                )
                            })?
                            .1
                            .content
                            .clone(),
                    )
                    .expect("from_value::<Raw<..>> can never fail")
                    .deserialize()
                    .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
        )?;

        // Presence update
        db.rooms.edus.update_presence(
            &sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(&sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(&sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: ruma::presence::PresenceState::Online,
                    status_msg: None,
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush().await?;

    Ok(set_display_name::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>/displayname", data = "<body>")
)]
pub async fn get_displayname_route(
    db: State<'_, Database>,
    body: Ruma<get_display_name::Request<'_>>,
) -> ConduitResult<get_display_name::Response> {
    Ok(get_display_name::Response {
        displayname: db.users.displayname(&body.user_id)?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/profile/<_>/avatar_url", data = "<body>")
)]
pub async fn set_avatar_url_route(
    db: State<'_, Database>,
    body: Ruma<set_avatar_url::Request<'_>>,
) -> ConduitResult<set_avatar_url::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.users
        .set_avatar_url(&sender_user, body.avatar_url.clone())?;

    // Send a new membership event and presence update into all joined rooms
    for room_id in db.rooms.rooms_joined(&sender_user) {
        let room_id = room_id?;
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(ruma::events::room::member::MemberEventContent {
                    avatar_url: body.avatar_url.clone(),
                    ..serde_json::from_value::<Raw<_>>(
                        db.rooms
                            .room_state_get(
                                &room_id,
                                &EventType::RoomMember,
                                &sender_user.to_string(),
                            )?
                            .ok_or_else(|| {
                                Error::bad_database(
                                    "Tried to send avatar url update for user not in the room.",
                                )
                            })?
                            .1
                            .content
                            .clone(),
                    )
                    .expect("from_value::<Raw<..>> can never fail")
                    .deserialize()
                    .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
        )?;

        // Presence update
        db.rooms.edus.update_presence(
            &sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(&sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(&sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: ruma::presence::PresenceState::Online,
                    status_msg: None,
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush().await?;

    Ok(set_avatar_url::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>/avatar_url", data = "<body>")
)]
pub async fn get_avatar_url_route(
    db: State<'_, Database>,
    body: Ruma<get_avatar_url::Request<'_>>,
) -> ConduitResult<get_avatar_url::Response> {
    Ok(get_avatar_url::Response {
        avatar_url: db.users.avatar_url(&body.user_id)?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>", data = "<body>")
)]
pub async fn get_profile_route(
    db: State<'_, Database>,
    body: Ruma<get_profile::Request<'_>>,
) -> ConduitResult<get_profile::Response> {
    if !db.users.exists(&body.user_id)? {
        // Return 404 if this user doesn't exist
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Profile was not found.",
        ));
    }

    Ok(get_profile::Response {
        avatar_url: db.users.avatar_url(&body.user_id)?,
        displayname: db.users.displayname(&body.user_id)?,
    }
    .into())
}
