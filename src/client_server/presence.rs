use crate::{database::DatabaseGuard, utils, Result, Ruma};
use ruma::api::client::presence::{get_presence, set_presence};
use std::time::Duration;

/// # `PUT /_matrix/client/r0/presence/{userId}/status`
///
/// Sets the presence state of the sender user.
pub async fn set_presence_route(
    db: DatabaseGuard,
    body: Ruma<set_presence::v3::Request<'_>>,
) -> Result<set_presence::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for room_id in db.rooms.rooms_joined(sender_user) {
        let room_id = room_id?;

        db.rooms.edus.update_presence(
            sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: body.presence.clone(),
                    status_msg: body.status_msg.clone(),
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(set_presence::v3::Response {})
}

/// # `GET /_matrix/client/r0/presence/{userId}/status`
///
/// Gets the presence state of the given user.
///
/// - Only works if you share a room with the user
pub async fn get_presence_route(
    db: DatabaseGuard,
    body: Ruma<get_presence::v3::Request<'_>>,
) -> Result<get_presence::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut presence_event = None;

    for room_id in db
        .rooms
        .get_shared_rooms(vec![sender_user.clone(), body.user_id.clone()])?
    {
        let room_id = room_id?;

        if let Some(presence) = db
            .rooms
            .edus
            .get_last_presence_event(sender_user, &room_id)?
        {
            presence_event = Some(presence);
            break;
        }
    }

    if let Some(presence) = presence_event {
        Ok(get_presence::v3::Response {
            // TODO: Should ruma just use the presenceeventcontent type here?
            status_msg: presence.content.status_msg,
            currently_active: presence.content.currently_active,
            last_active_ago: presence
                .content
                .last_active_ago
                .map(|millis| Duration::from_millis(millis.into())),
            presence: presence.content.presence,
        })
    } else {
        todo!();
    }
}
