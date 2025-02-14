use std::{cmp::max, collections::BTreeMap};

use axum::extract::State;
use conduit::{debug_info, debug_warn, err, Err};
use ruma::{
	api::client::{
		error::ErrorKind,
		room::{self, aliases, create_room, get_room_event, upgrade_room},
	},
	events::{
		room::{
			canonical_alias::RoomCanonicalAliasEventContent,
			create::RoomCreateEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
			tombstone::RoomTombstoneEventContent,
			topic::RoomTopicEventContent,
		},
		StateEventType, TimelineEventType,
	},
	int,
	serde::{JsonObject, Raw},
	CanonicalJsonObject, Int, OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, RoomVersionId,
};
use serde_json::{json, value::to_raw_value};
use tracing::{error, info, warn};

use super::invite_helper;
use crate::{
	service::{appservice::RegistrationInfo, pdu::PduBuilder, Services},
	Error, Result, Ruma,
};

/// Recommended transferable state events list from the spec
const TRANSFERABLE_STATE_EVENTS: &[StateEventType; 9] = &[
	StateEventType::RoomServerAcl,
	StateEventType::RoomEncryption,
	StateEventType::RoomName,
	StateEventType::RoomAvatar,
	StateEventType::RoomTopic,
	StateEventType::RoomGuestAccess,
	StateEventType::RoomHistoryVisibility,
	StateEventType::RoomJoinRules,
	StateEventType::RoomPowerLevels,
];

/// # `POST /_matrix/client/v3/createRoom`
///
/// Creates a new room.
///
/// - Room ID is randomly generated
/// - Create alias if `room_alias_name` is set
/// - Send create event
/// - Join sender user
/// - Send power levels event
/// - Send canonical room alias
/// - Send join rules
/// - Send history visibility
/// - Send guest access
/// - Send events listed in initial state
/// - Send events implied by `name` and `topic`
/// - Send invite events
#[allow(clippy::large_stack_frames)]
pub(crate) async fn create_room_route(
	State(services): State<crate::State>, body: Ruma<create_room::v3::Request>,
) -> Result<create_room::v3::Response> {
	use create_room::v3::RoomPreset;

	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	if !services.globals.allow_room_creation()
		&& body.appservice_info.is_none()
		&& !services.users.is_admin(sender_user)?
	{
		return Err(Error::BadRequest(ErrorKind::forbidden(), "Room creation has been disabled."));
	}

	let room_id: OwnedRoomId = if let Some(custom_room_id) = &body.room_id {
		custom_room_id_check(&services, custom_room_id)?
	} else {
		RoomId::new(&services.globals.config.server_name)
	};

	// check if room ID doesn't already exist instead of erroring on auth check
	if services.rooms.short.get_shortroomid(&room_id)?.is_some() {
		return Err(Error::BadRequest(
			ErrorKind::RoomInUse,
			"Room with that custom room ID already exists",
		));
	}

	if body.visibility == room::Visibility::Public
		&& services.globals.config.lockdown_public_room_directory
		&& !services.users.is_admin(sender_user)?
		&& body.appservice_info.is_none()
	{
		info!(
			"Non-admin user {sender_user} tried to publish {0} to the room directory while \
			 \"lockdown_public_room_directory\" is enabled",
			&room_id
		);

		if services.globals.config.admin_room_notices {
			services
				.admin
				.send_text(&format!(
					"Non-admin user {sender_user} tried to publish {0} to the room directory while \
					 \"lockdown_public_room_directory\" is enabled",
					&room_id
				))
				.await;
		}

		return Err!(Request(Forbidden("Publishing rooms to the room directory is not allowed")));
	}

	let _short_id = services.rooms.short.get_or_create_shortroomid(&room_id)?;
	let state_lock = services.rooms.state.mutex.lock(&room_id).await;

	let alias: Option<OwnedRoomAliasId> = if let Some(alias) = &body.room_alias_name {
		Some(room_alias_check(&services, alias, &body.appservice_info).await?)
	} else {
		None
	};

	let room_version = match body.room_version.clone() {
		Some(room_version) => {
			if services
				.globals
				.supported_room_versions()
				.contains(&room_version)
			{
				room_version
			} else {
				return Err(Error::BadRequest(
					ErrorKind::UnsupportedRoomVersion,
					"This server does not support that room version.",
				));
			}
		},
		None => services.globals.default_room_version(),
	};

	#[allow(clippy::single_match_else)]
	let content = match &body.creation_content {
		Some(content) => {
			use RoomVersionId::*;

			let mut content = content
				.deserialize_as::<CanonicalJsonObject>()
				.map_err(|e| {
					error!("Failed to deserialise content as canonical JSON: {}", e);
					Error::bad_database("Failed to deserialise content as canonical JSON.")
				})?;
			match room_version {
				V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
					content.insert(
						"creator".into(),
						json!(&sender_user).try_into().map_err(|e| {
							info!("Invalid creation content: {e}");
							Error::BadRequest(ErrorKind::BadJson, "Invalid creation content")
						})?,
					);
				},
				_ => {
					// V11+ removed the "creator" key
				},
			}
			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid creation content"))?,
			);
			content
		},
		None => {
			use RoomVersionId::*;

			let content = match room_version {
				V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => RoomCreateEventContent::new_v1(sender_user.clone()),
				_ => RoomCreateEventContent::new_v11(),
			};
			let mut content = serde_json::from_str::<CanonicalJsonObject>(
				to_raw_value(&content)
					.expect("we just created this as content was None")
					.get(),
			)
			.unwrap();
			content.insert(
				"room_version".into(),
				json!(room_version.as_str())
					.try_into()
					.expect("we just created this as content was None"),
			);
			content
		},
	};

	// 1. The room create event
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&content).expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 2. Let the room creator join
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomMember,
				content: to_raw_value(&RoomMemberEventContent {
					membership: MembershipState::Join,
					displayname: services.users.displayname(sender_user)?,
					avatar_url: services.users.avatar_url(sender_user)?,
					is_direct: Some(body.is_direct),
					third_party_invite: None,
					blurhash: services.users.blurhash(sender_user)?,
					reason: None,
					join_authorized_via_users_server: None,
				})
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(sender_user.to_string()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 3. Power levels

	// Figure out preset. We need it for preset specific events
	let preset = body.preset.clone().unwrap_or(match &body.visibility {
		room::Visibility::Public => RoomPreset::PublicChat,
		_ => RoomPreset::PrivateChat, // Room visibility should not be custom
	});

	let mut users = BTreeMap::from_iter([(sender_user.clone(), int!(100))]);

	if preset == RoomPreset::TrustedPrivateChat {
		for invite_ in &body.invite {
			users.insert(invite_.clone(), int!(100));
		}
	}

	let power_levels_content =
		default_power_levels_content(&body.power_level_content_override, &body.visibility, users)?;

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomPowerLevels,
				content: to_raw_value(&power_levels_content).expect("to_raw_value always works on serde_json::Value"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 4. Canonical room alias
	if let Some(room_alias_id) = &alias {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: TimelineEventType::RoomCanonicalAlias,
					content: to_raw_value(&RoomCanonicalAliasEventContent {
						alias: Some(room_alias_id.to_owned()),
						alt_aliases: vec![],
					})
					.expect("We checked that alias earlier, it must be fine"),
					unsigned: None,
					state_key: Some(String::new()),
					redacts: None,
					timestamp: None,
				},
				sender_user,
				&room_id,
				&state_lock,
			)
			.await?;
	}

	// 5. Events set by preset

	// 5.1 Join Rules
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomJoinRules,
				content: to_raw_value(&RoomJoinRulesEventContent::new(match preset {
					RoomPreset::PublicChat => JoinRule::Public,
					// according to spec "invite" is the default
					_ => JoinRule::Invite,
				}))
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 5.2 History Visibility
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomHistoryVisibility,
				content: to_raw_value(&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared))
					.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 5.3 Guest Access
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomGuestAccess,
				content: to_raw_value(&RoomGuestAccessEventContent::new(match preset {
					RoomPreset::PublicChat => GuestAccess::Forbidden,
					_ => GuestAccess::CanJoin,
				}))
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&room_id,
			&state_lock,
		)
		.await?;

	// 6. Events listed in initial_state
	for event in &body.initial_state {
		let mut pdu_builder = event.deserialize_as::<PduBuilder>().map_err(|e| {
			warn!("Invalid initial state event: {:?}", e);
			Error::BadRequest(ErrorKind::InvalidParam, "Invalid initial state event.")
		})?;

		debug_info!("Room creation initial state event: {event:?}");

		// client/appservice workaround: if a user sends an initial_state event with a
		// state event in there with the content of literally `{}` (not null or empty
		// string), let's just skip it over and warn.
		if pdu_builder.content.get().eq("{}") {
			info!("skipping empty initial state event with content of `{{}}`: {event:?}");
			debug_warn!("content: {}", pdu_builder.content.get());
			continue;
		}

		// Implicit state key defaults to ""
		pdu_builder.state_key.get_or_insert_with(String::new);

		// Silently skip encryption events if they are not allowed
		if pdu_builder.event_type == TimelineEventType::RoomEncryption && !services.globals.allow_encryption() {
			continue;
		}

		services
			.rooms
			.timeline
			.build_and_append_pdu(pdu_builder, sender_user, &room_id, &state_lock)
			.await?;
	}

	// 7. Events implied by name and topic
	if let Some(name) = &body.name {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: TimelineEventType::RoomName,
					content: to_raw_value(&RoomNameEventContent::new(name.clone()))
						.expect("event is valid, we just created it"),
					unsigned: None,
					state_key: Some(String::new()),
					redacts: None,
					timestamp: None,
				},
				sender_user,
				&room_id,
				&state_lock,
			)
			.await?;
	}

	if let Some(topic) = &body.topic {
		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: TimelineEventType::RoomTopic,
					content: to_raw_value(&RoomTopicEventContent {
						topic: topic.clone(),
					})
					.expect("event is valid, we just created it"),
					unsigned: None,
					state_key: Some(String::new()),
					redacts: None,
					timestamp: None,
				},
				sender_user,
				&room_id,
				&state_lock,
			)
			.await?;
	}

	// 8. Events implied by invite (and TODO: invite_3pid)
	drop(state_lock);
	for user_id in &body.invite {
		if let Err(e) = invite_helper(&services, sender_user, user_id, &room_id, None, body.is_direct).await {
			warn!(%e, "Failed to send invite");
		}
	}

	// Homeserver specific stuff
	if let Some(alias) = alias {
		services
			.rooms
			.alias
			.set_alias(&alias, &room_id, sender_user)?;
	}

	if body.visibility == room::Visibility::Public {
		services.rooms.directory.set_public(&room_id)?;

		if services.globals.config.admin_room_notices {
			services
				.admin
				.send_text(&format!("{sender_user} made {} public to the room directory", &room_id))
				.await;
		}
		info!("{sender_user} made {0} public to the room directory", &room_id);
	}

	info!("{sender_user} created a room with room ID {room_id}");

	Ok(create_room::v3::Response::new(room_id))
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/event/{eventId}`
///
/// Gets a single event.
///
/// - You have to currently be joined to the room (TODO: Respect history
///   visibility)
pub(crate) async fn get_room_event_route(
	State(services): State<crate::State>, body: Ruma<get_room_event::v3::Request>,
) -> Result<get_room_event::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	let event = services
		.rooms
		.timeline
		.get_pdu(&body.event_id)?
		.ok_or_else(|| err!(Request(NotFound("Event {} not found.", &body.event_id))))?;

	if !services
		.rooms
		.state_accessor
		.user_can_see_event(sender_user, &event.room_id, &body.event_id)?
	{
		return Err(Error::BadRequest(
			ErrorKind::forbidden(),
			"You don't have permission to view this event.",
		));
	}

	let mut event = (*event).clone();
	event.add_age()?;

	Ok(get_room_event::v3::Response {
		event: event.to_room_event(),
	})
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/aliases`
///
/// Lists all aliases of the room.
///
/// - Only users joined to the room are allowed to call this, or if
///   `history_visibility` is world readable in the room
pub(crate) async fn get_room_aliases_route(
	State(services): State<crate::State>, body: Ruma<aliases::v3::Request>,
) -> Result<aliases::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	if !services
		.rooms
		.state_accessor
		.user_can_see_state_events(sender_user, &body.room_id)?
	{
		return Err(Error::BadRequest(
			ErrorKind::forbidden(),
			"You don't have permission to view this room.",
		));
	}

	Ok(aliases::v3::Response {
		aliases: services
			.rooms
			.alias
			.local_aliases_for_room(&body.room_id)
			.filter_map(Result::ok)
			.collect(),
	})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/upgrade`
///
/// Upgrades the room.
///
/// - Creates a replacement room
/// - Sends a tombstone event into the current room
/// - Sender user joins the room
/// - Transfers some state events
/// - Moves local aliases
/// - Modifies old room power levels to prevent users from speaking
pub(crate) async fn upgrade_room_route(
	State(services): State<crate::State>, body: Ruma<upgrade_room::v3::Request>,
) -> Result<upgrade_room::v3::Response> {
	let sender_user = body.sender_user.as_ref().expect("user is authenticated");

	if !services
		.globals
		.supported_room_versions()
		.contains(&body.new_version)
	{
		return Err(Error::BadRequest(
			ErrorKind::UnsupportedRoomVersion,
			"This server does not support that room version.",
		));
	}

	// Create a replacement room
	let replacement_room = RoomId::new(services.globals.server_name());

	let _short_id = services
		.rooms
		.short
		.get_or_create_shortroomid(&replacement_room)?;

	let state_lock = services.rooms.state.mutex.lock(&body.room_id).await;

	// Send a m.room.tombstone event to the old room to indicate that it is not
	// intended to be used any further Fail if the sender does not have the required
	// permissions
	let tombstone_event_id = services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomTombstone,
				content: to_raw_value(&RoomTombstoneEventContent {
					body: "This room has been replaced".to_owned(),
					replacement_room: replacement_room.clone(),
				})
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&body.room_id,
			&state_lock,
		)
		.await?;

	// Change lock to replacement room
	drop(state_lock);
	let state_lock = services.rooms.state.mutex.lock(&replacement_room).await;

	// Get the old room creation event
	let mut create_event_content = serde_json::from_str::<CanonicalJsonObject>(
		services
			.rooms
			.state_accessor
			.room_state_get(&body.room_id, &StateEventType::RoomCreate, "")?
			.ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
			.content
			.get(),
	)
	.map_err(|_| Error::bad_database("Invalid room event in database."))?;

	// Use the m.room.tombstone event as the predecessor
	let predecessor = Some(ruma::events::room::create::PreviousRoom::new(
		body.room_id.clone(),
		(*tombstone_event_id).to_owned(),
	));

	// Send a m.room.create event containing a predecessor field and the applicable
	// room_version
	{
		use RoomVersionId::*;
		match body.new_version {
			V1 | V2 | V3 | V4 | V5 | V6 | V7 | V8 | V9 | V10 => {
				create_event_content.insert(
					"creator".into(),
					json!(&sender_user).try_into().map_err(|e| {
						info!("Error forming creation event: {e}");
						Error::BadRequest(ErrorKind::BadJson, "Error forming creation event")
					})?,
				);
			},
			_ => {
				// "creator" key no longer exists in V11+ rooms
				create_event_content.remove("creator");
			},
		}
	}

	create_event_content.insert(
		"room_version".into(),
		json!(&body.new_version)
			.try_into()
			.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
	);
	create_event_content.insert(
		"predecessor".into(),
		json!(predecessor)
			.try_into()
			.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
	);

	// Validate creation event content
	if serde_json::from_str::<CanonicalJsonObject>(
		to_raw_value(&create_event_content)
			.expect("Error forming creation event")
			.get(),
	)
	.is_err()
	{
		return Err(Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"));
	}

	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomCreate,
				content: to_raw_value(&create_event_content).expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&replacement_room,
			&state_lock,
		)
		.await?;

	// Join the new room
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomMember,
				content: to_raw_value(&RoomMemberEventContent {
					membership: MembershipState::Join,
					displayname: services.users.displayname(sender_user)?,
					avatar_url: services.users.avatar_url(sender_user)?,
					is_direct: None,
					third_party_invite: None,
					blurhash: services.users.blurhash(sender_user)?,
					reason: None,
					join_authorized_via_users_server: None,
				})
				.expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(sender_user.to_string()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&replacement_room,
			&state_lock,
		)
		.await?;

	// Replicate transferable state events to the new room
	for event_type in TRANSFERABLE_STATE_EVENTS {
		let event_content = match services
			.rooms
			.state_accessor
			.room_state_get(&body.room_id, event_type, "")?
		{
			Some(v) => v.content.clone(),
			None => continue, // Skipping missing events.
		};

		services
			.rooms
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: event_type.to_string().into(),
					content: event_content,
					unsigned: None,
					state_key: Some(String::new()),
					redacts: None,
					timestamp: None,
				},
				sender_user,
				&replacement_room,
				&state_lock,
			)
			.await?;
	}

	// Moves any local aliases to the new room
	for alias in services
		.rooms
		.alias
		.local_aliases_for_room(&body.room_id)
		.filter_map(Result::ok)
	{
		services
			.rooms
			.alias
			.remove_alias(&alias, sender_user)
			.await?;
		services
			.rooms
			.alias
			.set_alias(&alias, &replacement_room, sender_user)?;
	}

	// Get the old room power levels
	let mut power_levels_event_content: RoomPowerLevelsEventContent = serde_json::from_str(
		services
			.rooms
			.state_accessor
			.room_state_get(&body.room_id, &StateEventType::RoomPowerLevels, "")?
			.ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
			.content
			.get(),
	)
	.map_err(|_| Error::bad_database("Invalid room event in database."))?;

	// Setting events_default and invite to the greater of 50 and users_default + 1
	let new_level = max(
		int!(50),
		power_levels_event_content
			.users_default
			.checked_add(int!(1))
			.ok_or_else(|| {
				Error::BadRequest(ErrorKind::BadJson, "users_default power levels event content is not valid")
			})?,
	);
	power_levels_event_content.events_default = new_level;
	power_levels_event_content.invite = new_level;

	// Modify the power levels in the old room to prevent sending of events and
	// inviting new users
	services
		.rooms
		.timeline
		.build_and_append_pdu(
			PduBuilder {
				event_type: TimelineEventType::RoomPowerLevels,
				content: to_raw_value(&power_levels_event_content).expect("event is valid, we just created it"),
				unsigned: None,
				state_key: Some(String::new()),
				redacts: None,
				timestamp: None,
			},
			sender_user,
			&body.room_id,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	// Return the replacement room id
	Ok(upgrade_room::v3::Response {
		replacement_room,
	})
}

/// creates the power_levels_content for the PDU builder
fn default_power_levels_content(
	power_level_content_override: &Option<Raw<RoomPowerLevelsEventContent>>, visibility: &room::Visibility,
	users: BTreeMap<OwnedUserId, Int>,
) -> Result<serde_json::Value> {
	let mut power_levels_content = serde_json::to_value(RoomPowerLevelsEventContent {
		users,
		..Default::default()
	})
	.expect("event is valid, we just created it");

	// secure proper defaults of sensitive/dangerous permissions that moderators
	// (power level 50) should not have easy access to
	power_levels_content["events"]["m.room.power_levels"] = serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.server_acl"] = serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.tombstone"] = serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.encryption"] = serde_json::to_value(100).expect("100 is valid Value");
	power_levels_content["events"]["m.room.history_visibility"] =
		serde_json::to_value(100).expect("100 is valid Value");

	// always allow users to respond (not post new) to polls. this is primarily
	// useful in read-only announcement rooms that post a public poll.
	power_levels_content["events"]["org.matrix.msc3381.poll.response"] =
		serde_json::to_value(0).expect("0 is valid Value");
	power_levels_content["events"]["m.poll.response"] = serde_json::to_value(0).expect("0 is valid Value");

	// synapse does this too. clients do not expose these permissions. it prevents
	// default users from calling public rooms, for obvious reasons.
	if *visibility == room::Visibility::Public {
		power_levels_content["events"]["m.call.invite"] = serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["m.call"] = serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["m.call.member"] = serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["org.matrix.msc3401.call"] =
			serde_json::to_value(50).expect("50 is valid Value");
		power_levels_content["events"]["org.matrix.msc3401.call.member"] =
			serde_json::to_value(50).expect("50 is valid Value");
	}

	if let Some(power_level_content_override) = power_level_content_override {
		let json: JsonObject = serde_json::from_str(power_level_content_override.json().get())
			.map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override."))?;

		for (key, value) in json {
			power_levels_content[key] = value;
		}
	}

	Ok(power_levels_content)
}

/// if a room is being created with a room alias, run our checks
async fn room_alias_check(
	services: &Services, room_alias_name: &str, appservice_info: &Option<RegistrationInfo>,
) -> Result<OwnedRoomAliasId> {
	// Basic checks on the room alias validity
	if room_alias_name.contains(':') {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Room alias contained `:` which is not allowed. Please note that this expects a localpart, not the full \
			 room alias.",
		));
	} else if room_alias_name.contains(char::is_whitespace) {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Room alias contained spaces which is not a valid room alias.",
		));
	}

	// check if room alias is forbidden
	if services
		.globals
		.forbidden_alias_names()
		.is_match(room_alias_name)
	{
		return Err(Error::BadRequest(ErrorKind::Unknown, "Room alias name is forbidden."));
	}

	let full_room_alias = RoomAliasId::parse(format!("#{}:{}", room_alias_name, services.globals.config.server_name))
		.map_err(|e| {
		info!("Failed to parse room alias {room_alias_name}: {e}");
		Error::BadRequest(ErrorKind::InvalidParam, "Invalid room alias specified.")
	})?;

	if services
		.rooms
		.alias
		.resolve_local_alias(&full_room_alias)?
		.is_some()
	{
		return Err(Error::BadRequest(ErrorKind::RoomInUse, "Room alias already exists."));
	}

	if let Some(ref info) = appservice_info {
		if !info.aliases.is_match(full_room_alias.as_str()) {
			return Err(Error::BadRequest(ErrorKind::Exclusive, "Room alias is not in namespace."));
		}
	} else if services
		.appservice
		.is_exclusive_alias(&full_room_alias)
		.await
	{
		return Err(Error::BadRequest(ErrorKind::Exclusive, "Room alias reserved by appservice."));
	}

	debug_info!("Full room alias: {full_room_alias}");

	Ok(full_room_alias)
}

/// if a room is being created with a custom room ID, run our checks against it
fn custom_room_id_check(services: &Services, custom_room_id: &str) -> Result<OwnedRoomId> {
	// apply forbidden room alias checks to custom room IDs too
	if services
		.globals
		.forbidden_alias_names()
		.is_match(custom_room_id)
	{
		return Err(Error::BadRequest(ErrorKind::Unknown, "Custom room ID is forbidden."));
	}

	if custom_room_id.contains(':') {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Custom room ID contained `:` which is not allowed. Please note that this expects a localpart, not the \
			 full room ID.",
		));
	} else if custom_room_id.contains(char::is_whitespace) {
		return Err(Error::BadRequest(
			ErrorKind::InvalidParam,
			"Custom room ID contained spaces which is not valid.",
		));
	}

	let full_room_id = format!("!{}:{}", custom_room_id, services.globals.config.server_name);

	debug_info!("Full custom room ID: {full_room_id}");

	RoomId::parse(full_room_id).map_err(|e| {
		info!("User attempted to create room with custom room ID {custom_room_id} but failed parsing: {e}");
		Error::BadRequest(ErrorKind::InvalidParam, "Custom room ID could not be parsed")
	})
}
