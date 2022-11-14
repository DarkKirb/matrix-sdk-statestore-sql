//! Crypto store implementation

use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use dashmap::DashSet;
use educe::Educe;
use futures::{StreamExt, TryStream, TryStreamExt};
use matrix_sdk_base::{
    deserialized_responses::MemberEvent, locks::Mutex, MinimalRoomMemberEvent, RoomInfo,
};
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::{DeviceStore, GroupSessionStore, SessionStore},
        BackupKeys, Changes, CryptoStore, RecoveryKey, RoomKeyCounts,
    },
    CryptoStoreError, GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities,
    SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use parking_lot::RwLock;
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent,
    },
    serde::Raw,
    DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sqlx::{
    database::HasArguments, types::Json, ColumnIndex, Database, Executor, IntoArguments, Row,
    Transaction,
};

use crate::{
    helpers::{BorrowedSqlType, SqlType},
    Result, SQLStoreError, StateStore, SupportedDatabase,
};

/// Store Result type
type StoreResult<T> = Result<T, CryptoStoreError>;

/// Cryptostore data
#[derive(Educe)]
#[educe(Debug)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct CryptostoreData {
    /// Encryption cipher
    #[educe(Debug(ignore))]
    pub(crate) cipher: Option<StoreCipher>,
    /// Account info
    pub(crate) account: RwLock<Option<AccountInfo>>,
    /// In-Memory session store
    pub(crate) sessions: SessionStore,
    /// In-Memory group session store
    pub(crate) group_sessions: GroupSessionStore,
    /// In-Memory device store
    pub(crate) devices: DeviceStore,
    /// In-Memory tracked users cache
    pub(crate) tracked_users: Arc<DashSet<OwnedUserId>>,
    /// In-Memory key query cache
    pub(crate) users_for_key_query: Arc<DashSet<OwnedUserId>>,
}

impl CryptostoreData {
    /// Create a new cryptostore data
    pub(crate) fn new(cipher: StoreCipher) -> Self {
        Self {
            cipher: Some(cipher),
            account: RwLock::new(None),
            sessions: SessionStore::new(),
            group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
            tracked_users: Arc::new(DashSet::new()),
            users_for_key_query: Arc::new(DashSet::new()),
        }
    }

    /// Create a new unencrypted cryptostore data struct
    pub(crate) fn new_unencrypted() -> Self {
        Self {
            cipher: None,
            account: RwLock::new(None),
            sessions: SessionStore::new(),
            group_sessions: GroupSessionStore::new(),
            devices: DeviceStore::new(),
            tracked_users: Arc::new(DashSet::new()),
            users_for_key_query: Arc::new(DashSet::new()),
        }
    }

    /// Encode a key
    pub(crate) fn encode_key<'a>(&self, table_name: &str, key: &'a [u8]) -> Cow<'a, [u8]> {
        self.cipher.as_ref().map_or_else(
            || key.into(),
            |v| {
                v.hash_key(table_name.as_ref(), key.as_ref())
                    .to_vec()
                    .into()
            },
        )
    }

    /// Tries to encode a value
    ///
    /// # Errors
    /// This function returns an error if serialization or encryption fails.
    pub(crate) fn encode_value<T: Serialize>(&self, value: &T) -> Result<Vec<u8>> {
        if let Some(ref v) = self.cipher {
            let encrypted = v.encrypt_value_typed(value)?;
            Ok(bincode::serialize(&encrypted)?)
        } else {
            Ok(serde_json::to_vec(value)?)
        }
    }

    /// Tries to decode a value
    ///
    /// # Errors
    /// This function returns an error if deserialization or decryption fails.
    pub(crate) fn decode_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T> {
        if let Some(ref v) = self.cipher {
            let deser = bincode::deserialize(value)?;
            let decrypted = v.decrypt_value_typed(deser)?;
            Ok(decrypted)
        } else {
            Ok(serde_json::from_slice(value)?)
        }
    }
}
/// Account information
#[derive(Clone, Debug)]
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct AccountInfo {
    /// User ID of the current user
    user_id: Arc<UserId>,
    /// Device ID of the current device
    device_id: Arc<DeviceId>,
    /// Identity keys for the current user
    identity_keys: Arc<IdentityKeys>,
}

/// Tracked users
#[derive(Debug, Serialize, Deserialize)]
struct TrackedUser {
    /// User ID of tracked user
    user_id: OwnedUserId,
    /// Whether or not keys for the user need to be queried
    dirty: bool,
}

impl<DB: SupportedDatabase> StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
    for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    for<'a> &'a str: BorrowedSqlType<'a, DB>,
    Vec<u8>: SqlType<DB>,
    String: SqlType<DB>,
    bool: SqlType<DB>,
    Vec<u8>: SqlType<DB>,
    Option<String>: SqlType<DB>,
    Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
    Json<Raw<PresenceEvent>>: SqlType<DB>,
    Json<SyncRoomMemberEvent>: SqlType<DB>,
    Json<MinimalRoomMemberEvent>: SqlType<DB>,
    Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
    Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
    Json<RoomInfo>: SqlType<DB>,
    Json<Receipt>: SqlType<DB>,
    Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
    Json<StrippedRoomMemberEvent>: SqlType<DB>,
    Json<MemberEvent>: SqlType<DB>,
    for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
{
    /// Returns account info, if it exists
    #[cfg(test)]
    pub(crate) fn get_account_info(&self) -> Option<AccountInfo> {
        self.ensure_e2e()
            .map(|e| e.account.read().clone())
            .unwrap_or_default()
    }
    /// Loads tracked users
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_tracked_users(&self) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let mut rows = DB::tracked_users_fetch_query().fetch(&*self.db);
        while let Some(row) = rows.try_next().await? {
            let user: Vec<u8> = row.try_get("tracked_user_data")?;
            let user: TrackedUser = e2e.decode_value(&user)?;
            e2e.tracked_users.insert(user.user_id.clone());
            if user.dirty {
                e2e.users_for_key_query.insert(user.user_id.clone());
            }
        }
        Ok(())
    }

    /// Loads a previously stored account
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        let e2e = self.ensure_e2e()?;
        let account = match self.get_kv(b"e2e_account").await? {
            Some(account) => {
                let account = e2e.decode_value(&account)?;
                let account = ReadOnlyAccount::from_pickle(account)?;

                let account_info = AccountInfo {
                    user_id: Arc::clone(&account.user_id),
                    device_id: Arc::clone(&account.device_id),
                    identity_keys: Arc::clone(&account.identity_keys),
                };
                *(self.ensure_e2e()?.account.write()) = Some(account_info);

                Some(account)
            }
            None => None,
        };
        Ok(account)
    }

    /// Stores an account
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        let mut txn = self.db.begin().await?;
        self.save_account_txn(&mut txn, account).await?;
        txn.commit().await?;

        Ok(())
    }

    /// Stores an account in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_account_txn<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        account: ReadOnlyAccount,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let account_info = AccountInfo {
            user_id: Arc::clone(&account.user_id),
            device_id: Arc::clone(&account.device_id),
            identity_keys: Arc::clone(&account.identity_keys),
        };
        *(e2e.account.write()) = Some(account_info);
        Self::insert_kv_txn(
            txn,
            b"e2e_account",
            &e2e.encode_value(&account.pickle().await)?,
        )
        .await?;
        Ok(())
    }

    /// Loads the cross-signing identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        let e2e = self.ensure_e2e()?;
        let private_identity = match self.get_kv(b"private_identity").await? {
            Some(account) => {
                let private_identity = e2e.decode_value(&account)?;
                let private_identity = PrivateCrossSigningIdentity::from_pickle(private_identity)
                    .await
                    .map_err(|e| SQLStoreError::Sign(Box::new(e)))?;
                Some(private_identity)
            }
            None => None,
        };
        Ok(private_identity)
    }

    /// Stores the cross-signing identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_identity<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        identity: PrivateCrossSigningIdentity,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        Self::insert_kv_txn(
            txn,
            b"private_identity",
            &e2e.encode_value(&identity.pickle().await?)?,
        )
        .await?;
        Ok(())
    }

    /// Stores the backup version
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_backup_version<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        backup_version: String,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        Self::insert_kv_txn(txn, b"backup_version", &e2e.encode_value(&backup_version)?).await?;
        Ok(())
    }

    /// Stores the recovery key
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn store_recovery_key<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        recovery_key: RecoveryKey,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        Self::insert_kv_txn(txn, b"recovery_key", &e2e.encode_value(&recovery_key)?).await?;
        Ok(())
    }

    /// Saves an olm session to database
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: Session,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let sender_key = session.sender_key().to_base64();
        let sender_key = sender_key.as_bytes();
        let sender_key = e2e.encode_key("cryptostore_session:sender_key", sender_key);
        DB::session_store_query()
            .bind(sender_key.as_ref())
            .bind(e2e.encode_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.sessions.add(session).await;
        Ok(())
    }

    /// Saves an olm message hash
    ///
    /// # Errors
    /// This function will return an error if the query fails
    pub(crate) async fn save_message_hash<'c>(
        txn: &mut Transaction<'c, DB>,
        message_hash: OlmMessageHash,
    ) -> Result<()> {
        DB::olm_message_hash_store_query()
            .bind(message_hash.sender_key)
            .bind(message_hash.hash)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves an inbound group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_inbound_group_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: InboundGroupSession,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let room_id = e2e.encode_key(
            "cryptostore_inbound_group_session:room_id",
            session.room_id().as_bytes(),
        );
        let raw_key = session.sender_key.to_base64();
        let sender_key = e2e.encode_key(
            "cryptostore_inbound_group_session:sender_key",
            raw_key.as_bytes(),
        );
        let session_id = e2e.encode_key(
            "cryptostore_inbound_group_session:session_id",
            session.session_id().as_bytes(),
        );
        DB::inbound_group_session_upsert_query()
            .bind(room_id.as_ref())
            .bind(sender_key.as_ref())
            .bind(session_id.as_ref())
            .bind(e2e.encode_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.group_sessions.add(session);
        Ok(())
    }

    /// Saves an outbound group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_outbound_group_session<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        session: OutboundGroupSession,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let room_id = e2e.encode_key(
            "cryptostore_inbound_group_session:room_id",
            session.room_id().as_bytes(),
        );
        DB::outbound_group_session_store_query()
            .bind(room_id.as_ref())
            .bind(e2e.encode_value(&session.pickle().await)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a gossip request
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_gossip_request<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        request: GossipRequest,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let recipient_id = e2e.encode_key(
            "cryptostore_gossip_request:recipient_id",
            request.request_recipient.as_bytes(),
        );
        let request_id = e2e.encode_key(
            "cryptostore_gossip_request:request_id",
            request.request_id.as_bytes(),
        );
        let request_info_key = request.info.as_key();
        let info_key = e2e.encode_key(
            "cryptostore_gossip_request:info_key",
            request_info_key.as_bytes(),
        );
        DB::gossip_request_store_query()
            .bind(recipient_id.as_ref())
            .bind(request_id.as_ref())
            .bind(info_key.as_ref())
            .bind(request.sent_out)
            .bind(e2e.encode_value(&request)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a cryptographic identity
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_crypto_identity<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        identity: ReadOnlyUserIdentities,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key(
            "cryptostore_identity:user_id",
            identity.user_id().as_bytes(),
        );
        DB::identity_upsert_query()
            .bind(user_id.as_ref())
            .bind(e2e.encode_value(&identity)?)
            .execute(txn)
            .await?;
        Ok(())
    }

    /// Saves a device
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_device<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        device: ReadOnlyDevice,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_device:user_id", device.user_id().as_bytes());
        let device_id = e2e.encode_key(
            "cryptostore_device:device_id",
            device.device_id().as_bytes(),
        );
        DB::device_upsert_query()
            .bind(user_id.as_ref())
            .bind(device_id.as_ref())
            .bind(e2e.encode_value(&device)?)
            .execute(txn)
            .await?;
        self.ensure_e2e()?.devices.add(device);
        Ok(())
    }

    /// Deletes a device
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn delete_device<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        device: ReadOnlyDevice,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_device:user_id", device.user_id().as_bytes());
        let device_id = e2e.encode_key(
            "cryptostore_device:device_id",
            device.device_id().as_bytes(),
        );
        DB::device_delete_query()
            .bind(user_id.as_ref())
            .bind(device_id.as_ref())
            .execute(txn)
            .await?;
        self.ensure_e2e()?
            .devices
            .remove(device.user_id(), device.device_id());
        Ok(())
    }

    /// Applies cryptostore changes to the database in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_changes_txn<'c>(
        &self,
        txn: &mut Transaction<'c, DB>,
        changes: Changes,
    ) -> Result<()> {
        if let Some(account) = changes.account {
            self.save_account_txn(txn, account).await?;
        }
        if let Some(identity) = changes.private_identity {
            self.store_identity(txn, identity).await?;
        }
        if let Some(backup_version) = changes.backup_version {
            self.store_backup_version(txn, backup_version).await?;
        }
        if let Some(recovery_key) = changes.recovery_key {
            self.store_recovery_key(txn, recovery_key).await?;
        }
        for session in changes.sessions {
            self.save_session(txn, session).await?;
        }
        for message_hash in changes.message_hashes {
            Self::save_message_hash(txn, message_hash).await?;
        }
        for session in changes.inbound_group_sessions {
            self.save_inbound_group_session(txn, session).await?;
        }
        for session in changes.outbound_group_sessions {
            self.save_outbound_group_session(txn, session).await?;
        }
        for request in changes.key_requests {
            self.save_gossip_request(txn, request).await?;
        }
        for identity_change in changes
            .identities
            .changed
            .into_iter()
            .chain(changes.identities.new.into_iter())
        {
            self.save_crypto_identity(txn, identity_change).await?;
        }

        for device in changes
            .devices
            .changed
            .into_iter()
            .chain(changes.devices.new.into_iter())
        {
            self.save_device(txn, device).await?;
        }

        for device in changes.devices.deleted {
            self.delete_device(txn, device).await?;
        }

        Ok(())
    }

    /// Applies cryptostore changes to the database
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_changes(&self, changes: Changes) -> Result<()> {
        let mut txn = self.db.begin().await?;
        self.save_changes_txn(&mut txn, changes).await?;
        txn.commit().await?;
        Ok(())
    }

    /// Retrieve the sessions for a sender key
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let e2e = self.ensure_e2e()?;
        let sessions = &e2e.sessions;
        if let Some(v) = sessions.get(sender_key) {
            Ok(Some(v))
        } else {
            let account_info = e2e.account.read().clone();
            let account_info = account_info
                .as_ref()
                .ok_or(SQLStoreError::MissingAccountInfo)?;
            // try fetching from the database
            let user_id = e2e.encode_key("cryptostore_session:sender_key", sender_key.as_bytes());
            let mut rows = DB::sessions_for_user_query()
                .bind(user_id.as_ref())
                .fetch(&*self.db);
            let mut sess = Vec::new();
            while let Some(row) = rows.try_next().await? {
                let data: Vec<u8> = row.try_get("session_data")?;
                let session = e2e.decode_value(&data)?;
                let session = Session::from_pickle(
                    Arc::clone(&account_info.user_id),
                    Arc::clone(&account_info.device_id),
                    Arc::clone(&account_info.identity_keys),
                    session,
                );
                sessions.add(session.clone()).await;
                sess.push(session);
            }
            Ok(sessions.get(sender_key))
        }
    }

    /// Retrieve an incoming group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let e2e = self.ensure_e2e()?;
        let sessions = &e2e.group_sessions;
        if let Some(v) = sessions.get(room_id, sender_key, session_id) {
            Ok(Some(v))
        } else {
            let room_id = e2e.encode_key(
                "cryptostore_inbound_group_session:room_id",
                room_id.as_bytes(),
            );
            let sender_key = e2e.encode_key(
                "cryptostore_inbound_group_session:sender_key",
                sender_key.as_bytes(),
            );
            let session_id = e2e.encode_key(
                "cryptostore_inbound_group_session:session_id",
                session_id.as_bytes(),
            );
            let row = DB::inbound_group_session_fetch_query()
                .bind(room_id.as_ref())
                .bind(sender_key.as_ref())
                .bind(session_id.as_ref())
                .fetch_optional(&*self.db)
                .await?;
            if let Some(row) = row {
                let data: Vec<u8> = row.try_get("session_data")?;
                let session = e2e.decode_value(&data)?;
                let session = InboundGroupSession::from_pickle(session)?;
                sessions.add(session.clone());
                Ok(Some(session))
            } else {
                Ok(None)
            }
        }
    }

    /// Fetch all inbound group sessions
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked.
    pub(crate) fn get_inbound_group_session_stream(
        &self,
    ) -> Result<impl TryStream<Ok = InboundGroupSession, Error = SQLStoreError> + '_> {
        let e2e = self.ensure_e2e()?;
        Ok(DB::inbound_group_sessions_fetch_query()
            .fetch(&*self.db)
            .map_err(Into::into)
            .and_then(move |row| {
                let result = move || {
                    let data: Vec<u8> = row.try_get("session_data")?;
                    let session = e2e.decode_value(&data)?;
                    let session = InboundGroupSession::from_pickle(session)?;
                    Ok(session)
                };
                futures::future::ready((result)())
            }))
    }

    /// Fetch all inbound group sessions in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked.
    pub(crate) fn get_inbound_group_session_stream_txn<'r, 'c>(
        &'r self,
        txn: &'r mut Transaction<'c, DB>,
    ) -> Result<impl TryStream<Ok = InboundGroupSession, Error = SQLStoreError> + 'r> {
        let e2e = self.ensure_e2e()?;
        Ok(Box::pin(
            DB::inbound_group_sessions_fetch_query()
                .fetch(txn)
                .map_err(Into::into)
                .and_then(move |row| {
                    let result = move || {
                        let data: Vec<u8> = row.try_get("session_data")?;
                        let session = e2e.decode_value(&data)?;
                        let session = InboundGroupSession::from_pickle(session)?;
                        Ok(session)
                    };
                    futures::future::ready((result)())
                }),
        ))
    }

    /// Fetch all inbound group sessions
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        self.get_inbound_group_session_stream()?.try_collect().await
    }

    /// Fetch inbound session counts
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        self.get_inbound_group_session_stream()?
            .try_fold(RoomKeyCounts::default(), |mut counts, session| async move {
                counts.total += 1;
                if session.backed_up() {
                    counts.backed_up += 1;
                }
                Ok(counts)
            })
            .await
    }

    /// Fetch inbound group sessions for backup
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        self.get_inbound_group_session_stream()?
            .try_filter(|v| futures::future::ready(!v.backed_up()))
            .take(limit)
            .try_collect()
            .await
    }

    /// Resets the backup state of all inbound group sessions
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn reset_backup_state(&self) -> Result<()> {
        let mut txn = self.db.begin().await?;
        let sessions: Vec<_> = self
            .get_inbound_group_session_stream_txn(&mut txn)?
            .try_collect()
            .await?;
        for session in sessions {
            session.reset_backup_state();
            self.save_inbound_group_session(&mut txn, session).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    /// Loads the saved backup keys
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let e2e = self.ensure_e2e()?;
        let backup_version = self
            .get_kv(b"backup_version")
            .await?
            .map(|v| e2e.decode_value(&v).map_err(SQLStoreError::from))
            .transpose()?;
        let recovery_key = self
            .get_kv(b"recovery_key")
            .await?
            .map(|v| e2e.decode_value(&v).map_err(SQLStoreError::from))
            .transpose()?;
        Ok(BackupKeys {
            recovery_key,
            backup_version,
        })
    }

    /// Retrieve an outbound group session
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let e2e = self.ensure_e2e()?;
        let account_info = e2e.account.read().clone();
        let account_info = account_info
            .as_ref()
            .ok_or(SQLStoreError::MissingAccountInfo)?;
        let room_id = e2e.encode_key(
            "cryptostore_inbound_group_session:room_id",
            room_id.as_bytes(),
        );
        let row = DB::outbound_group_session_load_query()
            .bind(room_id.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        if let Some(row) = row {
            let data: Vec<u8> = row.try_get("session_data")?;
            let session = e2e.decode_value(&data)?;
            let session = OutboundGroupSession::from_pickle(
                Arc::clone(&account_info.device_id),
                Arc::clone(&account_info.identity_keys),
                session,
            )?;
            Ok(Some(session))
        } else {
            Ok(None)
        }
    }

    /// Saves a tracked user in a transaction
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn save_tracked_user(&self, tracked_user: &UserId, dirty: bool) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_tracked_user:user_id", tracked_user.as_bytes());
        let tracked_user = TrackedUser {
            user_id: tracked_user.into(),
            dirty,
        };
        DB::tracked_user_upsert_query()
            .bind(user_id.as_ref())
            .bind(e2e.encode_value(&tracked_user)?)
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Update a tracked user
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let e2e = self.ensure_e2e()?;
        let already_added = e2e.tracked_users.insert(user.to_owned());

        if dirty {
            e2e.users_for_key_query.insert(user.to_owned());
        } else {
            e2e.users_for_key_query.remove(user);
        }

        self.save_tracked_user(user, dirty).await?;

        Ok(already_added)
    }

    /// Fetch a device
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_device:user_id", user_id.as_bytes());
        let device_id = e2e.encode_key("cryptostore_device:device_id", device_id.as_bytes());
        let row = DB::device_fetch_query()
            .bind(user_id.as_ref())
            .bind(device_id.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        if let Some(row) = row {
            let data: Vec<u8> = row.try_get("device_info")?;
            let device = e2e.decode_value(&data)?;
            Ok(Some(device))
        } else {
            Ok(None)
        }
    }

    /// Fetch devices for a user
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_device:user_id", user_id.as_bytes());
        let mut rows = DB::devices_for_user_query()
            .bind(user_id.as_ref())
            .fetch(&*self.db);
        let mut devices = HashMap::new();
        while let Some(row) = rows.try_next().await? {
            let data: Vec<u8> = row.try_get("device_info")?;
            let device: ReadOnlyDevice = e2e.decode_value(&data)?;
            let device_id = device.device_id().to_owned();
            devices.insert(device_id, device);
        }
        Ok(devices)
    }

    /// Fetch cryptographic identity of a user
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<ReadOnlyUserIdentities>> {
        let e2e = self.ensure_e2e()?;
        let user_id = e2e.encode_key("cryptostore_identity:user_id", user_id.as_bytes());
        let row = DB::identity_fetch_query()
            .bind(user_id.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        if let Some(row) = row {
            let data: Vec<u8> = row.try_get("identity_data")?;
            let identity = e2e.decode_value(&data)?;
            Ok(Some(identity))
        } else {
            Ok(None)
        }
    }

    /// Check if a message hash is known
    ///
    /// # Errors
    /// This function will return an error if the query fails
    pub(crate) async fn is_message_known(&self, message_hash: &OlmMessageHash) -> Result<bool> {
        let row = DB::message_known_query()
            .bind(message_hash.sender_key.clone())
            .bind(message_hash.hash.clone())
            .fetch_optional(&*self.db)
            .await?;
        Ok(row.is_some())
    }

    /// Retrieves an outgoing key request
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_outgoing_key_request(
        &self,
        id: &[u8],
    ) -> Result<Option<GossipRequest>> {
        let e2e = self.ensure_e2e()?;
        let id = e2e.encode_key("cryptostore_gossip_request:request_id", id);
        let row = DB::gossip_request_fetch_query()
            .bind(id.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        if let Some(row) = row {
            let data: Vec<u8> = row.try_get("gossip_data")?;
            let request = e2e.decode_value(&data)?;
            Ok(Some(request))
        } else {
            Ok(None)
        }
    }

    /// Retrieves an outgoing key request by info
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let e2e = self.ensure_e2e()?;
        let request_info_key = key_info.as_key();
        let info_key = e2e.encode_key(
            "cryptostore_gossip_request:info_key",
            request_info_key.as_bytes(),
        );
        let row = DB::gossip_request_info_fetch_query()
            .bind(info_key.as_ref())
            .fetch_optional(&*self.db)
            .await?;
        if let Some(row) = row {
            let data: Vec<u8> = row.try_get("gossip_data")?;
            let request = e2e.decode_value(&data)?;
            Ok(Some(request))
        } else {
            Ok(None)
        }
    }

    /// Retrieves unsent outgoing key requests
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        let e2e = self.ensure_e2e()?;
        let mut rows = DB::gossip_requests_sent_state_fetch_query()
            .bind(false)
            .fetch(&*self.db);
        let mut requests = Vec::new();
        while let Some(row) = rows.try_next().await? {
            let data: Vec<u8> = row.try_get("gossip_data")?;
            let request = e2e.decode_value(&data)?;
            requests.push(request);
        }
        Ok(requests)
    }

    /// Deletes outgoing key requests
    ///
    /// # Errors
    /// This function will return an error if the database has not been unlocked,
    /// or if the query fails.
    pub(crate) async fn delete_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<()> {
        let e2e = self.ensure_e2e()?;
        let id = e2e.encode_key(
            "cryptostore_gossip_request:request_id",
            request_id.as_str().as_bytes(),
        );
        DB::gossip_request_delete_query()
            .bind(id.as_ref())
            .execute(&*self.db)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl<DB: SupportedDatabase> CryptoStore for StateStore<DB>
where
    for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
    for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
    for<'c, 'a> &'a mut Transaction<'c, DB>: Executor<'a, Database = DB>,
    for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    for<'a> &'a str: BorrowedSqlType<'a, DB>,
    Vec<u8>: SqlType<DB>,
    String: SqlType<DB>,
    bool: SqlType<DB>,
    Vec<u8>: SqlType<DB>,
    Option<String>: SqlType<DB>,
    Json<Raw<AnyGlobalAccountDataEvent>>: SqlType<DB>,
    Json<Raw<PresenceEvent>>: SqlType<DB>,
    Json<SyncRoomMemberEvent>: SqlType<DB>,
    Json<MinimalRoomMemberEvent>: SqlType<DB>,
    Json<Raw<AnySyncStateEvent>>: SqlType<DB>,
    Json<Raw<AnyRoomAccountDataEvent>>: SqlType<DB>,
    Json<RoomInfo>: SqlType<DB>,
    Json<Receipt>: SqlType<DB>,
    Json<Raw<AnyStrippedStateEvent>>: SqlType<DB>,
    Json<StrippedRoomMemberEvent>: SqlType<DB>,
    Json<MemberEvent>: SqlType<DB>,
    for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
{
    async fn load_account(&self) -> StoreResult<Option<ReadOnlyAccount>> {
        self.load_account()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn save_account(&self, account: ReadOnlyAccount) -> StoreResult<()> {
        self.save_account(account)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn load_identity(&self) -> StoreResult<Option<PrivateCrossSigningIdentity>> {
        self.load_identity()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn save_changes(&self, changes: Changes) -> StoreResult<()> {
        self.save_changes(changes)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> StoreResult<Option<Arc<Mutex<Vec<Session>>>>> {
        self.get_sessions(sender_key)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> StoreResult<Option<InboundGroupSession>> {
        self.get_inbound_group_session(room_id, sender_key, session_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_inbound_group_sessions(&self) -> StoreResult<Vec<InboundGroupSession>> {
        self.get_inbound_group_sessions()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn inbound_group_session_counts(&self) -> StoreResult<RoomKeyCounts> {
        self.inbound_group_session_counts()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> StoreResult<Vec<InboundGroupSession>> {
        self.inbound_group_sessions_for_backup(limit)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn reset_backup_state(&self) -> StoreResult<()> {
        self.reset_backup_state()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn load_backup_keys(&self) -> StoreResult<BackupKeys> {
        self.load_backup_keys()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<Option<OutboundGroupSession>> {
        self.get_outbound_group_sessions(room_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.ensure_e2e()
            .map(|e2e| e2e.tracked_users.contains(user_id))
            .unwrap_or(false)
    }
    fn has_users_for_key_query(&self) -> bool {
        self.ensure_e2e()
            .map(|e2e| !e2e.users_for_key_query.is_empty())
            .unwrap_or(false)
    }
    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        self.ensure_e2e()
            .map(|e2e| e2e.users_for_key_query.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }
    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.ensure_e2e()
            .map(|e2e| e2e.tracked_users.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }
    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> StoreResult<bool> {
        self.update_tracked_user(user, dirty)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> StoreResult<Option<ReadOnlyDevice>> {
        self.get_device(user_id, device_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> StoreResult<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        self.get_user_devices(user_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<ReadOnlyUserIdentities>> {
        self.get_user_identity(user_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn is_message_known(&self, message_hash: &OlmMessageHash) -> StoreResult<bool> {
        self.is_message_known(message_hash)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> StoreResult<Option<GossipRequest>> {
        self.get_outgoing_key_request(request_id.as_str().as_bytes())
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_secret_request_by_info(
        &self,
        secret_info: &SecretInfo,
    ) -> StoreResult<Option<GossipRequest>> {
        self.get_secret_request_by_info(secret_info)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn get_unsent_secret_requests(&self) -> StoreResult<Vec<GossipRequest>> {
        self.get_unsent_secret_requests()
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> StoreResult<()> {
        self.delete_outgoing_secret_requests(request_id)
            .await
            .map_err(|e| CryptoStoreError::Backend(e.into()))
    }
}

#[allow(clippy::redundant_pub_crate)]
#[cfg(all(test, feature = "postgres", feature = "ci"))]
mod postgres_integration_test {
    use std::sync::Arc;

    use crate::StateStore;

    use matrix_sdk_crypto::{
        cryptostore_integration_tests, olm::OutboundGroupSession, EncryptionSettings,
    };
    use matrix_sdk_test::async_test;
    use ruma::{device_id, room_id};
    use sqlx::migrate::MigrateDatabase;
    use vodozemac::olm::Account;

    async fn get_store_result(
        name: String,
        passphrase: Option<&str>,
    ) -> crate::Result<StateStore<sqlx::postgres::Postgres>> {
        let db_url = format!("postgres://postgres:postgres@localhost:5432/{}", name);
        if !sqlx::Postgres::database_exists(&db_url).await? {
            sqlx::Postgres::create_database(&db_url).await?;
        }
        let pass = passphrase.unwrap_or("default_test_password");
        let db = Arc::new(sqlx::PgPool::connect(&db_url).await?);
        let mut store = StateStore::new(&db).await?;
        store.unlock_with_passphrase(pass).await?;
        Ok(store)
    }

    #[allow(clippy::panic)]
    async fn get_store(
        name: String,
        passphrase: Option<&str>,
    ) -> StateStore<sqlx::postgres::Postgres> {
        match get_store_result(name, passphrase).await {
            Ok(v) => v,
            Err(e) => {
                panic!("Could not open database: {:#?}", e);
            }
        }
    }

    #[async_test]
    #[allow(clippy::unwrap_used)]
    async fn cryptostore_outbound_group_session() {
        let store = get_store("cryptostore_outbound_group_session".to_owned(), None).await;
        for _ in 0..2 {
            let mut txn = store.db.begin().await.unwrap();
            let outbound_group_session = OutboundGroupSession::new(
                From::from(device_id!("ALICEDEVICE")),
                Arc::new(Account::new().identity_keys()),
                room_id!("!test:localhost"),
                EncryptionSettings::default(),
            );
            store
                .save_outbound_group_session(&mut txn, outbound_group_session)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }
    }

    cryptostore_integration_tests!();
}

#[allow(clippy::redundant_pub_crate)]
#[cfg(all(test, feature = "sqlite"))]
mod sqlite_integration_test {
    use std::sync::Arc;

    use crate::StateStore;

    use matrix_sdk_crypto::{
        cryptostore_integration_tests, olm::OutboundGroupSession, EncryptionSettings,
    };
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{device_id, room_id};
    use sqlx::migrate::MigrateDatabase;
    use tempfile::{tempdir, TempDir};
    use vodozemac::olm::Account;

    #[allow(clippy::unwrap_used)]
    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());

    async fn get_store_result(
        name: &str,
        passphrase: Option<&str>,
    ) -> crate::Result<StateStore<sqlx::sqlite::Sqlite>> {
        let tmpdir_path = TMP_DIR.path().join(name.to_owned() + ".db");
        let db_url = format!("sqlite://{}", tmpdir_path.to_string_lossy());
        if !sqlx::Sqlite::database_exists(&db_url).await? {
            sqlx::Sqlite::create_database(&db_url).await?;
        }
        let pass = passphrase.unwrap_or("default_test_password");
        let db = Arc::new(sqlx::SqlitePool::connect(&db_url).await?);
        let mut store = StateStore::new(&db).await?;
        store.unlock_with_passphrase(pass).await?;
        Ok(store)
    }

    #[allow(clippy::panic)]
    async fn get_store(name: &str, passphrase: Option<&str>) -> StateStore<sqlx::sqlite::Sqlite> {
        match get_store_result(name, passphrase).await {
            Ok(v) => v,
            Err(e) => {
                panic!("Could not open database: {:#?}", e);
            }
        }
    }

    #[async_test]
    #[allow(clippy::unwrap_used)]
    async fn cryptostore_outbound_group_session() {
        let store = get_store("cryptostore_outbound_group_session", None).await;
        for _ in 0..2 {
            let mut txn = store.db.begin().await.unwrap();
            let outbound_group_session = OutboundGroupSession::new(
                From::from(device_id!("ALICEDEVICE")),
                Arc::new(Account::new().identity_keys()),
                room_id!("!test:localhost"),
                EncryptionSettings::default(),
            )
            .unwrap();
            store
                .save_outbound_group_session(&mut txn, outbound_group_session)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        }
    }

    cryptostore_integration_tests!();
}
