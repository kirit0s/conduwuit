use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    appservice_server, database::pusher, server_server, utils, Database, Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;
use log::{info, warn};
use ring::digest;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{
    api::{appservice, federation, OutgoingRequest},
    events::{push_rules, EventType},
    uint, ServerName, UInt, UserId,
};
use sled::IVec;
use tokio::{select, sync::Semaphore};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(Box<ServerName>),
    Push(Vec<u8>, Vec<u8>), // user and pushkey
    Normal(Box<ServerName>),
}

#[derive(Clone)]
pub struct Sending {
    /// The state for a given state hash.
    pub(super) servernamepduids: sled::Tree, // ServernamePduId = (+ / $)SenderKey / ServerName / UserId + PduId
    pub(super) servercurrentpdus: sled::Tree, // ServerCurrentPdus = (+ / $)ServerName / UserId + PduId (pduid can be empty for reservation)
    pub(super) maximum_requests: Arc<Semaphore>,
}

impl Sending {
    pub fn start_handler(&self, db: &Database) {
        let servernamepduids = self.servernamepduids.clone();
        let servercurrentpdus = self.servercurrentpdus.clone();

        let db = db.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            // Retry requests we could not finish yet
            let mut current_transactions = HashMap::<OutgoingKind, Vec<IVec>>::new();

            for (key, outgoing_kind, pdu) in servercurrentpdus
                .iter()
                .filter_map(|r| r.ok())
                .filter_map(|(key, _)| {
                    Self::parse_servercurrentpdus(&key)
                        .ok()
                        .map(|(k, p)| (key, k, p))
                })
            {
                if pdu.is_empty() {
                    // Remove old reservation key
                    servercurrentpdus.remove(key).unwrap();
                    continue;
                }

                let entry = current_transactions
                    .entry(outgoing_kind)
                    .or_insert_with(Vec::new);

                if entry.len() > 30 {
                    warn!("Dropping some current pdus because too many were queued. This should not happen.");
                    servercurrentpdus.remove(key).unwrap();
                    continue;
                }

                entry.push(pdu);
            }

            for (outgoing_kind, pdus) in current_transactions {
                // Create new reservation
                let mut prefix = match &outgoing_kind {
                    OutgoingKind::Appservice(server) => {
                        let mut p = b"+".to_vec();
                        p.extend_from_slice(server.as_bytes());
                        p
                    }
                    OutgoingKind::Push(user, pushkey) => {
                        let mut p = b"$".to_vec();
                        p.extend_from_slice(&user);
                        p.push(0xff);
                        p.extend_from_slice(&pushkey);
                        p
                    }
                    OutgoingKind::Normal(server) => {
                        let mut p = Vec::new();
                        p.extend_from_slice(server.as_bytes());
                        p
                    }
                };
                prefix.push(0xff);
                servercurrentpdus.insert(prefix, &[]).unwrap();

                futures.push(Self::handle_event(outgoing_kind.clone(), pdus, &db));
            }

            let mut last_failed_try: HashMap<OutgoingKind, (u32, Instant)> = HashMap::new();

            let mut subscriber = servernamepduids.watch_prefix(b"");
            loop {
                select! {
                    Some(response) = futures.next() => {
                        match response {
                            Ok(outgoing_kind) => {
                                let mut prefix = match &outgoing_kind {
                                    OutgoingKind::Appservice(server) => {
                                        let mut p = b"+".to_vec();
                                        p.extend_from_slice(server.as_bytes());
                                        p
                                    }
                                    OutgoingKind::Push(user, pushkey) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&user);
                                        p.push(0xff);
                                        p.extend_from_slice(&pushkey);
                                        p
                                    },
                                    OutgoingKind::Normal(server) => {
                                        let mut p = vec![];
                                        p.extend_from_slice(server.as_bytes());
                                        p
                                    },
                                };
                                prefix.push(0xff);

                                for key in servercurrentpdus
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                {
                                    // Don't remove reservation yet
                                    if prefix.len() != key.len() {
                                        servercurrentpdus.remove(key).unwrap();
                                    }
                                }

                                // Find events that have been added since starting the last request
                                let new_pdus = servernamepduids
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                    .map(|k| {
                                        k.subslice(prefix.len(), k.len() - prefix.len())
                                    })
                                    .take(30)
                                    .collect::<Vec<_>>();

                                if !new_pdus.is_empty() {
                                    for pdu_id in &new_pdus {
                                        let mut current_key = prefix.clone();
                                        current_key.extend_from_slice(pdu_id);
                                        servercurrentpdus.insert(&current_key, &[]).unwrap();
                                        servernamepduids.remove(&current_key).unwrap();
                                    }

                                    futures.push(
                                        Self::handle_event(
                                            outgoing_kind.clone(),
                                            new_pdus,
                                            &db,
                                        )
                                    );
                                } else {
                                    servercurrentpdus.remove(&prefix).unwrap();
                                    // servercurrentpdus with the prefix should be empty now
                                }
                            }
                            Err((outgoing_kind, e)) => {
                                info!("Couldn't send transaction to {:?}\n{}", outgoing_kind, e);
                                let mut prefix = match &outgoing_kind {
                                    OutgoingKind::Appservice(serv) => {
                                        let mut p = b"+".to_vec();
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                    OutgoingKind::Push(user, pushkey) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&user);
                                        p.push(0xff);
                                        p.extend_from_slice(&pushkey);
                                        p
                                    },
                                    OutgoingKind::Normal(serv) => {
                                        let mut p = vec![];
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                };

                                prefix.push(0xff);

                                last_failed_try.insert(outgoing_kind.clone(), match last_failed_try.get(&outgoing_kind) {
                                    Some(last_failed) => {
                                        (last_failed.0+1, Instant::now())
                                    },
                                    None => {
                                        (1, Instant::now())
                                    }
                                });
                                servercurrentpdus.remove(&prefix).unwrap();
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                            let servernamepduid = key.clone();

                            let exponential_backoff = |(tries, instant): &(u32, Instant)| {
                                // Fail if a request has failed recently (exponential backoff)
                                let mut min_elapsed_duration = Duration::from_secs(60) * (*tries) * (*tries);
                                if min_elapsed_duration > Duration::from_secs(60*60*24) {
                                    min_elapsed_duration = Duration::from_secs(60*60*24);
                                }

                                instant.elapsed() < min_elapsed_duration
                            };

                            if let Some((outgoing_kind, pdu_id)) = Self::parse_servercurrentpdus(&servernamepduid)
                                .ok()
                                .filter(|(outgoing_kind, _)| {
                                    if last_failed_try.get(outgoing_kind).map_or(false, exponential_backoff) {
                                        return false;
                                    }

                                    let mut prefix = match outgoing_kind {
                                        OutgoingKind::Appservice(serv) => {
                                            let mut p = b"+".to_vec();
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                    },
                                        OutgoingKind::Push(user, pushkey) => {
                                            let mut p = b"$".to_vec();
                                            p.extend_from_slice(&user);
                                            p.push(0xff);
                                            p.extend_from_slice(&pushkey);
                                            p
                                        },
                                        OutgoingKind::Normal(serv) => {
                                            let mut p = vec![];
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                        },
                                    };
                                    prefix.push(0xff);

                                    servercurrentpdus
                                        .compare_and_swap(prefix, Option::<&[u8]>::None, Some(&[])) // Try to reserve
                                        == Ok(Ok(()))
                                })
                            {
                                servercurrentpdus.insert(&key, &[]).unwrap();
                                servernamepduids.remove(&key).unwrap();

                                futures.push(
                                    Self::handle_event(
                                        outgoing_kind,
                                        vec![pdu_id],
                                        &db,
                                    )
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    #[tracing::instrument(skip(self))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], senderkey: IVec) -> Result<()> {
        let mut key = b"$".to_vec();
        key.extend_from_slice(&senderkey);
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = b"+".to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument]
    fn calculate_hash(keys: &[IVec]) -> Vec<u8> {
        // We only hash the pdu's event ids, not the whole pdu
        let bytes = keys.join(&0xff);
        let hash = digest::digest(&digest::SHA256, &bytes);
        hash.as_ref().to_owned()
    }

    #[tracing::instrument(skip(db))]
    async fn handle_event(
        kind: OutgoingKind,
        pdu_ids: Vec<IVec>,
        db: &Database,
    ) -> std::result::Result<OutgoingKind, (OutgoingKind, Error)> {
        match &kind {
            OutgoingKind::Appservice(server) => {
                let pdu_jsons = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (Box<ServerName>, Error)>(
                            db.rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (server.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        server.clone(),
                                        Error::bad_database(
                                            "[Appservice] Event in servernamepduids not found in ",
                                        ),
                                    )
                                })?
                                .to_any_event(),
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();
                let permit = db.sending.maximum_requests.acquire().await;

                let response = appservice_server::send_request(
                    &db.globals,
                    db.appservice
                        .get_registration(server.as_str())
                        .unwrap()
                        .unwrap(), // TODO: handle error
                    appservice::event::push_events::v1::Request {
                        events: &pdu_jsons,
                        txn_id: &base64::encode_config(
                            Self::calculate_hash(&pdu_ids),
                            base64::URL_SAFE_NO_PAD,
                        ),
                    },
                )
                .await
                .map(|_response| kind.clone())
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
            OutgoingKind::Push(user, pushkey) => {
                let pdus = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (Vec<u8>, Error)>(
                            db.rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (pushkey.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        pushkey.clone(),
                                        Error::bad_database(
                                            "[Push] Event in servernamepduids not found in db.",
                                        ),
                                    )
                                })?,
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();

                for pdu in pdus {
                    // Redacted events are not notification targets (we don't send push for them)
                    if pdu.unsigned.get("redacted_because").is_some() {
                        continue;
                    }

                    let userid =
                        UserId::try_from(utils::string_from_bytes(user).map_err(|_| {
                            (
                                OutgoingKind::Push(user.clone(), pushkey.clone()),
                                Error::bad_database("Invalid push user string in db."),
                            )
                        })?)
                        .map_err(|_| {
                            (
                                OutgoingKind::Push(user.clone(), pushkey.clone()),
                                Error::bad_database("Invalid push user id in db."),
                            )
                        })?;

                    let mut senderkey = user.clone();
                    senderkey.push(0xff);
                    senderkey.extend_from_slice(pushkey);

                    let pusher = match db
                        .pusher
                        .get_pusher(&senderkey)
                        .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                    {
                        Some(pusher) => pusher,
                        None => continue,
                    };

                    let rules_for_user = db
                        .account_data
                        .get::<push_rules::PushRulesEvent>(None, &userid, EventType::PushRules)
                        .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                        .map(|ev| ev.content.global)
                        .unwrap_or_else(|| crate::push_rules::default_pushrules(&userid));

                    let unread: UInt = if let Some(last_read) = db
                        .rooms
                        .edus
                        .private_read_get(&pdu.room_id, &userid)
                        .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                    {
                        (db.rooms
                            .pdus_since(&userid, &pdu.room_id, last_read)
                            .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                            .filter_map(|pdu| pdu.ok()) // Filter out buggy events
                            .filter(|(_, pdu)| {
                                matches!(
                                    pdu.kind.clone(),
                                    EventType::RoomMessage | EventType::RoomEncrypted
                                )
                            })
                            .count() as u32)
                            .into()
                    } else {
                        // Just return zero unread messages
                        uint!(0)
                    };

                    let permit = db.sending.maximum_requests.acquire().await;

                    let _response = pusher::send_push_notice(
                        &userid,
                        unread,
                        &pusher,
                        rules_for_user,
                        &pdu,
                        db,
                    )
                    .await
                    .map(|_response| kind.clone())
                    .map_err(|e| (kind.clone(), e));

                    drop(permit);
                }
                Ok(OutgoingKind::Push(user.clone(), pushkey.clone()))
            }
            OutgoingKind::Normal(server) => {
                let pdu_jsons = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (OutgoingKind, Error)>(
                            // TODO: check room version and remove event_id if needed
                            serde_json::from_str(
                                PduEvent::convert_to_outgoing_federation_event(
                                    db.rooms
                                        .get_pdu_json_from_id(pdu_id)
                                        .map_err(|e| (OutgoingKind::Normal(server.clone()), e))?
                                        .ok_or_else(|| {
                                            (
                                                OutgoingKind::Normal(server.clone()),
                                                Error::bad_database(
                                                    "[Normal] Event in servernamepduids not found in db.",
                                                ),
                                            )
                                        })?,
                                )
                                .json()
                                .get(),
                            )
                            .expect("Raw<..> is always valid"),
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();

                let permit = db.sending.maximum_requests.acquire().await;

                let response = server_server::send_request(
                    &db.globals,
                    &*server,
                    send_transaction_message::v1::Request {
                        origin: db.globals.server_name(),
                        pdus: &pdu_jsons,
                        edus: &[],
                        origin_server_ts: SystemTime::now(),
                        transaction_id: &base64::encode_config(
                            Self::calculate_hash(&pdu_ids),
                            base64::URL_SAFE_NO_PAD,
                        ),
                    },
                )
                .await
                .map(|response| {
                    info!("server response: {:?}", response);
                    kind.clone()
                })
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
        }
    }

    fn parse_servercurrentpdus(key: &IVec) -> Result<(OutgoingKind, IVec)> {
        // Appservices start with a plus
        Ok::<_, Error>(if key.starts_with(b"+") {
            let mut parts = key[1..].splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Appservice(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        } else if key.starts_with(b"$") {
            let mut parts = key[1..].splitn(3, |&b| b == 0xff);

            let user = parts.next().expect("splitn always returns one element");
            let pushkey = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            (
                OutgoingKind::Push(user.to_vec(), pushkey.to_vec()),
                IVec::from(pdu),
            )
        } else {
            let mut parts = key.splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Normal(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        })
    }

    #[tracing::instrument(skip(self, globals))]
    pub async fn send_federation_request<T: OutgoingRequest>(
        &self,
        globals: &crate::database::globals::Globals,
        destination: &ServerName,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = server_server::send_request(globals, destination, request).await;
        drop(permit);

        response
    }

    #[tracing::instrument(skip(self, globals))]
    pub async fn send_appservice_request<T: OutgoingRequest>(
        &self,
        globals: &crate::database::globals::Globals,
        registration: serde_yaml::Value,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = appservice_server::send_request(globals, registration, request).await;
        drop(permit);

        response
    }
}
