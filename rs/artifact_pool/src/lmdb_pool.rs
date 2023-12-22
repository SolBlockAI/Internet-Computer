use crate::consensus_pool::{InitializablePoolSection, PoolSectionOp, PoolSectionOps};
use crate::lmdb_iterator::{LMDBEcdsaIterator, LMDBIterator};
use crate::metrics::EcdsaPoolMetrics;
use ic_config::artifact_pool::LMDBConfig;
use ic_interfaces::{
    consensus_pool::{
        HeightIndexedPool, HeightRange, OnlyError, PoolSection, ValidatedConsensusArtifact,
    },
    ecdsa::{EcdsaPoolSection, EcdsaPoolSectionOp, EcdsaPoolSectionOps, MutableEcdsaPoolSection},
};
use ic_logger::{error, info, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_protobuf::types::v1 as pb;
use ic_types::consensus::certification::CertificationMessageHash;
use ic_types::{
    artifact::{CertificationMessageId, ConsensusMessageId, EcdsaMessageId},
    batch::BatchPayload,
    consensus::{
        certification::{Certification, CertificationMessage, CertificationShare},
        dkg,
        ecdsa::{
            EcdsaArtifactId, EcdsaComplaint, EcdsaMessage, EcdsaMessageType, EcdsaOpening,
            EcdsaPrefix, EcdsaPrefixOf, EcdsaSigShare,
        },
        BlockPayload, BlockProposal, CatchUpPackage, CatchUpPackageShare, ConsensusMessage,
        ConsensusMessageHash, ConsensusMessageHashable, Finalization, FinalizationShare, HasHeight,
        Notarization, NotarizationShare, Payload, PayloadType, RandomBeacon, RandomBeaconShare,
        RandomTape, RandomTapeShare,
    },
    crypto::canister_threshold_sig::idkg::{IDkgDealingSupport, SignedIDkgDealing},
    crypto::{CryptoHash, CryptoHashOf, CryptoHashable},
    Height, Time,
};
use lmdb::{
    Cursor, Database, DatabaseFlags, Environment, EnvironmentFlags, RoTransaction, RwTransaction,
    Transaction, WriteFlags,
};
use prost::Message;
use serde::{Deserialize, Serialize};
use std::convert::{TryFrom, TryInto};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::{os::raw::c_uint, path::Path, sync::Arc};
use strum::IntoEnumIterator;

/// Implementation of a persistent, height indexed pool using LMDB.
///
/// Given an artifact, we calculate 3 keys: TypeKey, HeightKey and IdKey,
/// where TypeKey only depends on the type of an artifact, and IdKey
/// is prefixed by height value (in big endian), which makes them
/// ordered and can be purged by heights.
///
/// There are 3 kind of LMDB databases used:
///
/// 1. An "artifacts" database maps IdKey to bincode encoded bytes for fast
/// serialization and deserialization:
///
/// ```text
/// artifacts
/// --------------------------------------
/// | IdKey | (bincode serialized) Bytes |
/// --------------------------------------
/// ```
///
/// 2. A set of index databases, one for each message type. Each one of them
/// maps a HeightKey to a set of IdKeys:
///
/// ```text
/// --------------------------
/// | HeightKey | IdKey, ... |
/// --------------------------
/// ```
///
/// 3. A "meta" database maps each TypeKey to the metadata of this message type,
///    which at the moment is only the min and max height.
///
/// ```text
/// meta
/// ------------------
/// | TypeKey | Meta |
/// ------------------
/// ```
pub struct PersistentHeightIndexedPool<T> {
    pool_type: PhantomData<T>,
    db_env: Arc<Environment>,
    meta: Database,
    artifacts: Database,
    indices: Vec<(TypeKey, Database)>,
    log: ReplicaLogger,
}

/// A trait for loading/saving pool artifacts (of ArtifactKind). It allows a
/// flexible data schema to be used for pool objects. For example, objects may
/// be normalized and serialized into multiple data entries, and they are
/// re-constructed upon loading. This can be taken care of by the `save` and
/// `load_as` interface, subject to the actual implementation for each
/// ArtifactKind.
///
/// We differentiate between 3 types:
///
/// 1. Object that is serialized and stored in the pool. This can include
/// additional data such as timestamp.
///
/// 2. Artifact::Message is the message type (usually an enum) of each
/// ArtifactKind. It can be casted into individual messages using TryFrom.
///
/// 3. Individual message type.
pub trait PoolArtifact: Sized {
    /// Type of the object to store.
    type ObjectType;
    type Id;

    /// The set of TypeKeys, one for each individual message type.
    /// This should be a const function.
    fn type_keys() -> &'static [TypeKey];

    /// Save an artifact to the database.
    fn save(
        key: &IdKey,
        value: Self::ObjectType,
        artifacts: Database,
        tx: &mut RwTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<()>;

    /// Load an artifact from the database. This is parameterized
    /// by the individual message type T.
    fn load_as<T: TryFrom<Self>>(
        key: &IdKey,
        db_env: Arc<Environment>,
        artifacts: Database,
        tx: &RoTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<T>
    where
        <T as TryFrom<Self>>::Error: Debug;
}

/// A unique representation for each type of supported message.
/// Internally it is just a const string.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct TypeKey {
    name: &'static str,
}

impl TypeKey {
    const fn new(name: &'static str) -> TypeKey {
        TypeKey { name }
    }
}

impl AsRef<[u8]> for TypeKey {
    fn as_ref(&self) -> &[u8] {
        self.name.as_bytes()
    }
}

/// Each support message gives a TypeKey.
pub trait HasTypeKey {
    fn type_key() -> TypeKey;
}

/// Message id as Key. The first 8 bytes is the big-endian representation
/// of the height, and the rest is hash.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct IdKey(Vec<u8>);

impl IdKey {
    pub fn height(&self) -> Height {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(&self.0[0..8]);
        Height::from(u64::from_be_bytes(bytes))
    }

    #[allow(unused)]
    pub fn hash(&self) -> CryptoHash {
        CryptoHash(self.0[8..].to_vec())
    }
}

impl AsRef<[u8]> for IdKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for IdKey {
    fn from(bytes: &[u8]) -> IdKey {
        IdKey(bytes.to_vec())
    }
}

impl From<(Height, &CryptoHash)> for IdKey {
    fn from((height, hash): (Height, &CryptoHash)) -> IdKey {
        let hash_bytes = &hash.0;
        let len = hash_bytes.len() + 8;
        let mut bytes: Vec<u8> = vec![0; len];
        let (left, right) = bytes.split_at_mut(8);
        left.copy_from_slice(&u64::to_be_bytes(height.get()));
        right.copy_from_slice(hash_bytes);
        IdKey(bytes)
    }
}

// This conversion is lossy because height and type tag are not preserved.
// It is okay because we don't expect reverse conversion.
impl From<&ConsensusMessageId> for IdKey {
    fn from(id: &ConsensusMessageId) -> IdKey {
        IdKey::from((id.height, id.hash.digest()))
    }
}

/// Height key.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug, Serialize, Deserialize)]
pub(crate) struct HeightKey([u8; 8]);

impl AsRef<[u8]> for HeightKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<&[u8]> for HeightKey {
    fn from(bytes: &[u8]) -> HeightKey {
        let mut bs: [u8; 8] = [0; 8];
        bs.copy_from_slice(bytes);
        HeightKey(bs)
    }
}

/// Use big-endian to ensure byte ordering.
impl From<Height> for HeightKey {
    fn from(height: Height) -> HeightKey {
        HeightKey(u64::to_be_bytes(height.get()))
    }
}

impl From<HeightKey> for Height {
    fn from(key: HeightKey) -> Height {
        Height::from(u64::from_be_bytes(key.0))
    }
}

/// DB Meta info about each message type is their min and max height
/// (inclusive).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    min: HeightKey,
    max: HeightKey,
}

/// Macro that logs the error when result is not Ok.
macro_rules! log_err {
    ($r:expr, $log:expr, $reason:expr) => {
        $r.map_err(|err| error!($log, "Error in DB operation {}: {:?}", $reason, err))
            .ok()
    };
}

/// Combination of type/height/id keys.
#[derive(Debug)]
struct ArtifactKey {
    type_key: TypeKey,
    height_key: HeightKey,
    id_key: IdKey,
}

/// Like log_err, but won't log the error if it matches the given error code.
macro_rules! log_err_except {
    ($r:expr, $log:expr, $code:pat, $reason:expr) => {
        $r.map_err(|err| match err {
            $code => {}
            _ => error!($log, "Error in DB operation {:?}: {:?}", $reason, err),
        })
        .ok()
    };
}

/// The max size (in bytes) of a persistent pool, also know as the LMDB map
/// size. It is a constant because it cannot be changed once DB is created.
const MAX_PERSISTENT_POOL_SIZE: usize = 0x0010_0000_0000; // 64GB

/// Max number of DB readers.
const MAX_READERS: c_uint = 2048;

fn create_db_env(path: &Path, read_only: bool, max_dbs: c_uint) -> Environment {
    let mut builder = Environment::new();
    let mut builder_flags = EnvironmentFlags::NO_TLS;
    let mut permission = 0o644;
    if read_only {
        builder_flags |= EnvironmentFlags::READ_ONLY;
        builder_flags |= EnvironmentFlags::NO_LOCK;
        permission = 0o444;
    }
    builder.set_flags(builder_flags);
    builder.set_max_readers(MAX_READERS);
    builder.set_max_dbs(max_dbs);
    builder.set_map_size(MAX_PERSISTENT_POOL_SIZE);
    let db_env = builder
        .open_with_permissions(path, permission)
        .unwrap_or_else(|err| {
            panic!(
                "Error opening LMDB environment with permissions at {:?}: {:?}",
                path, err
            )
        });

    unsafe {
        // Mark fds created by lmdb as FD_CLOEXEC to prevent them from leaking into
        // canister sandbox process. Details in NODE-166
        let mut fd: lmdb_sys::mdb_filehandle_t = lmdb_sys::mdb_filehandle_t::default();
        lmdb_sys::mdb_env_get_fd(db_env.env(), &mut fd);
        nix::fcntl::fcntl(fd, nix::fcntl::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC))
            .expect("Unable to mark FD_CLOEXEC");
    };
    db_env
}

///////////////////////////// Generic Pool /////////////////////////////

/// Collection of generic pool functions, indexed by Artifact type.
impl<Artifact: PoolArtifact> PersistentHeightIndexedPool<Artifact> {
    /// Return a persistent pool located the given directory path.
    /// Create the pool if it does not already exist.
    /// Panic if initialization fails.
    fn new(
        path: &Path,
        read_only: bool,
        log: ReplicaLogger,
    ) -> PersistentHeightIndexedPool<Artifact> {
        let type_keys = Artifact::type_keys();
        let db_env = create_db_env(path, read_only, (type_keys.len() + 2) as c_uint);

        // Create all databases.
        let meta = if read_only {
            db_env
                .open_db(Some("META"))
                .unwrap_or_else(|err| panic!("Error opening db for metadata: {:?}", err))
        } else {
            db_env
                .create_db(Some("META"), DatabaseFlags::empty())
                .unwrap_or_else(|err| panic!("Error creating db for metadata: {:?}", err))
        };
        let artifacts = if read_only {
            db_env
                .open_db(Some("ARTS"))
                .unwrap_or_else(|err| panic!("Error opening db for artifacts: {:?}", err))
        } else {
            db_env
                .create_db(Some("ARTS"), DatabaseFlags::empty())
                .unwrap_or_else(|err| panic!("Error creating db for artifacts: {:?}", err))
        };
        let indices = {
            type_keys
                .iter()
                .map(|type_key| {
                    // Use DUP_SORT to enable multi-value for each HeightKey.
                    let store = if read_only {
                        db_env.open_db(Some(type_key.name)).unwrap_or_else(|err| {
                            panic!("Error opening db {}: {:?}", type_key.name, err)
                        })
                    } else {
                        db_env
                            .create_db(Some(type_key.name), DatabaseFlags::DUP_SORT)
                            .unwrap_or_else(|err| {
                                panic!("Error creating db {}: {:?}", type_key.name, err)
                            })
                    };
                    (*type_key, store)
                })
                .collect()
        };
        Self {
            pool_type: PhantomData,
            db_env: Arc::new(db_env),
            meta,
            artifacts,
            indices,
            log,
        }
    }

    /// Update the meta data of the given type_key.
    fn update_meta(
        &self,
        tx: &mut RwTransaction,
        type_key: &TypeKey,
        meta: &Meta,
    ) -> lmdb::Result<()> {
        if let Some(bytes) = log_err!(
            bincode::serialize::<Meta>(meta),
            self.log,
            "update_meta serialize"
        ) {
            tx.put(self.meta, &type_key, &bytes, WriteFlags::empty())
        } else {
            Err(lmdb::Error::Panic)
        }
    }

    /// Get the meta data of the given type_key.
    fn get_meta<Tx: Transaction>(&self, tx: &mut Tx, type_key: &TypeKey) -> Option<Meta> {
        log_err_except!(
            tx.get(self.meta, &type_key),
            self.log,
            lmdb::Error::NotFound,
            format!("get_meta {:?}", type_key)
        )
        .and_then(|bytes| bincode::deserialize::<Meta>(bytes).ok())
    }

    /// Get the index database of the given type_key.
    /// Each index database maps HeightKey to a list of IdKey.
    fn get_index_db(&self, type_key: &TypeKey) -> Database {
        self.indices
            .iter()
            .find(|(key, _)| type_key == key)
            .unwrap_or_else(|| panic!("Error in get_index_db: {:?} does not exist", type_key))
            .1
    }

    /// Iterate messages between min and max HeightKey (inclusive).
    ///
    /// It is parameterized by an individual message type as long as it can be
    /// casted from the main `Artifact::Message` type.
    fn iterate<Message: TryFrom<Artifact> + HasTypeKey + 'static>(
        &self,
        min_key: HeightKey,
        max_key: HeightKey,
    ) -> Box<dyn Iterator<Item = Message>>
    where
        <Message as TryFrom<Artifact>>::Error: Debug,
    {
        let type_key = Message::type_key();
        let index_db = self.get_index_db(&type_key);
        let db_env = self.db_env.clone();
        let log = self.log.clone();
        let artifacts = self.artifacts;
        Box::new(LMDBIterator::new(
            db_env.clone(),
            index_db,
            min_key,
            max_key,
            move |tx: &RoTransaction<'_>, key: &[u8]| {
                Artifact::load_as::<Message>(&IdKey::from(key), db_env.clone(), artifacts, tx, &log)
            },
            self.log.clone(),
        ))
    }

    /// Insert a pool object under the given type/height/id key.
    fn tx_insert<PoolObject>(
        &self,
        tx: &mut RwTransaction,
        key: &ArtifactKey,
        value: PoolObject,
    ) -> lmdb::Result<()>
    where
        Artifact: PoolArtifact<ObjectType = PoolObject>,
    {
        self.tx_insert_prepare(tx, key)?;
        Artifact::save(&key.id_key, value, self.artifacts, tx, &self.log)
    }

    /// Prepares pool for artifact insertion, by checking index DB for duplicates and
    /// updating the metadata.
    fn tx_insert_prepare<PoolObject>(
        &self,
        tx: &mut RwTransaction,
        key: &ArtifactKey,
    ) -> lmdb::Result<()>
    where
        Artifact: PoolArtifact<ObjectType = PoolObject>,
    {
        // update index db first, because requiring NO_DUP_DATA may lead to
        // error when dup is detected. Insertion can be skipped in this case.
        let index_db = self.get_index_db(&key.type_key);
        tx.put(
            index_db,
            &key.height_key,
            &key.id_key,
            WriteFlags::NO_DUP_DATA,
        )?;
        // update meta
        let meta = self
            .get_meta(tx, &key.type_key)
            .map(|meta| Meta {
                min: meta.min.min(key.height_key),
                max: meta.max.max(key.height_key),
            })
            .unwrap_or(Meta {
                min: key.height_key,
                max: key.height_key,
            });
        self.update_meta(tx, &key.type_key, &meta)
    }

    /// Remove the pool object of the given type/height/id key.
    fn tx_remove(&self, tx: &mut RwTransaction, key: &ArtifactKey) -> lmdb::Result<()> {
        if let Err(err) = tx.del(self.artifacts, &key.id_key, None) {
            // skip the removal if it is not found in artifacts
            return if lmdb::Error::NotFound == err {
                Ok(())
            } else {
                Err(err)
            };
        }
        let index_db = self.get_index_db(&key.type_key);
        tx.del(index_db, &key.height_key, Some(&key.id_key.0))?;

        let min_height;
        let max_height;
        {
            let mut cursor = tx.open_ro_cursor(index_db)?;
            let mut iter = cursor.iter_start();
            min_height = iter
                .next()
                .transpose()?
                .map(|(key, _)| HeightKey::from(key));
            max_height = iter
                .last()
                .transpose()?
                .map(|(key, _)| HeightKey::from(key));
        }
        match (min_height, max_height) {
            (Some(min), Some(max)) => self.update_meta(tx, &key.type_key, &Meta { min, max }),
            (Some(min), None) => self.update_meta(tx, &key.type_key, &Meta { min, max: min }),
            _ => tx.del(self.meta, &key.type_key, None),
        }
    }

    /// Remove all index entries for the given [`TypeKey`] with heights
    /// less than the given [`HeightKey`]. Update the type's meta table
    /// if necessary. Return the [`ArtifactKey`]s of deleted entries.
    fn tx_purge_index_below(
        &self,
        tx: &mut RwTransaction,
        type_key: TypeKey,
        height_key: HeightKey,
    ) -> lmdb::Result<Vec<ArtifactKey>> {
        let mut artifact_ids = Vec::new();
        // only delete if meta exists
        if let Some(meta) = self.get_meta(tx, &type_key) {
            // nothing to delete if min height is already higher
            if meta.min >= height_key {
                return Ok(artifact_ids);
            }
            let index_db = self.get_index_db(&type_key);
            {
                let mut cursor = tx.open_rw_cursor(index_db)?;
                loop {
                    match cursor.iter().next().transpose()? {
                        None => break,
                        Some((key, id)) => {
                            if HeightKey::from(key) >= height_key {
                                break;
                            }
                            artifact_ids.push(ArtifactKey {
                                type_key,
                                height_key: HeightKey::from(key),
                                id_key: IdKey::from(id),
                            });
                            cursor.del(WriteFlags::empty())?;
                        }
                    }
                }
            }
            // update meta
            let meta = if meta.max <= height_key {
                None
            } else {
                let mut cursor = tx.open_rw_cursor(index_db)?;
                cursor
                    .iter_start()
                    .next()
                    .transpose()?
                    .map(|(key, _)| Meta {
                        min: HeightKey::from(key),
                        max: meta.max,
                    })
            };
            match meta {
                None => tx.del(self.meta, &type_key, None)?,
                Some(meta) => self.update_meta(tx, &type_key, &meta)?,
            }
        }
        Ok(artifact_ids)
    }

    /// Remove all artifacts with heights less than the given [`HeightKey`].
    /// Return [`ArtifactKey`]s of the removed artifacts.
    fn tx_purge_below(
        &self,
        tx: &mut RwTransaction,
        height_key: HeightKey,
    ) -> lmdb::Result<Vec<ArtifactKey>> {
        let mut purged = Vec::new();
        // delete from all index tables
        for &type_key in Artifact::type_keys() {
            purged.append(&mut self.tx_purge_index_below(tx, type_key, height_key)?);
        }
        // delete from artifacts table
        let mut cursor = tx.open_rw_cursor(self.artifacts)?;
        let height = Height::from(height_key);
        loop {
            match cursor.iter().next().transpose()? {
                None => break,
                Some((key, _)) => {
                    let id_key = IdKey::from(key);
                    if id_key.height() >= height {
                        break;
                    }
                    cursor.del(WriteFlags::empty())?;
                }
            }
        }
        Ok(purged)
    }

    /// Remove all artifacts of the given [`TypeKey`] with heights less than the
    /// given [`HeightKey`]. Return [`ArtifactKey`]s of the removed artifacts.
    fn tx_purge_type_below(
        &self,
        tx: &mut RwTransaction,
        type_key: TypeKey,
        height_key: HeightKey,
    ) -> lmdb::Result<Vec<ArtifactKey>> {
        let artifact_keys = self.tx_purge_index_below(tx, type_key, height_key)?;
        // delete the corresponding artifacts, ignoring not found errors
        for key in &artifact_keys {
            if let Err(err) = tx.del(self.artifacts, &key.id_key, None) {
                // Ignore not found errors, although they should not appear in practice.
                if lmdb::Error::NotFound != err {
                    return Err(err);
                }
            }
        }
        Ok(artifact_keys)
    }
}

impl InitializablePoolSection for PersistentHeightIndexedPool<ConsensusMessage> {
    /// Insert a cup with the original bytes from which that cup was received.
    fn insert_cup_with_proto(&self, cup_proto: pb::CatchUpPackage) {
        let cup = CatchUpPackage::try_from(&cup_proto).expect("deserializing CUP failed");
        let mut tx = self
            .db_env
            .begin_rw_txn()
            .expect("Unable to begin transaction to initialize consensus pool");
        let key = ArtifactKey::from(cup.get_id());

        // convert cup to bytes
        let bytes = &pb::ValidatedConsensusArtifact {
            msg: Some(pb::ConsensusMessage {
                msg: Some(pb::consensus_message::Msg::Cup(cup_proto)),
            }),
            timestamp: cup
                .content
                .block
                .as_ref()
                .context
                .time
                .as_nanos_since_unix_epoch(),
        }
        .encode_to_vec();

        // insert raw bytes
        self.tx_insert_prepare(&mut tx, &key)
            .expect("Insertion of metadata or updating index failed");
        tx.put(self.artifacts, &key.id_key, bytes, WriteFlags::empty())
            .expect("Insertion of CUP into initial consensus pool failed");
        tx.commit()
            .expect("Transaction inserting initial CUP into pool failed to commit");
    }
}

impl<Artifact: PoolArtifact, Message> HeightIndexedPool<Message>
    for PersistentHeightIndexedPool<Artifact>
where
    Message: TryFrom<Artifact> + HasTypeKey + 'static,
    <Message as TryFrom<Artifact>>::Error: Debug,
{
    fn height_range(&self) -> Option<HeightRange> {
        let mut tx = log_err!(self.db_env.begin_ro_txn(), self.log, "begin_ro_txn")?;
        self.get_meta(&mut tx, &Message::type_key())
            .map(|meta| HeightRange::new(Height::from(meta.min), Height::from(meta.max)))
    }

    fn max_height(&self) -> Option<Height> {
        <dyn HeightIndexedPool<Message>>::height_range(self).map(|range| range.max)
    }

    fn get_all(&self) -> Box<dyn Iterator<Item = Message>> {
        match <dyn HeightIndexedPool<Message>>::height_range(self) {
            None => Box::new(std::iter::empty()),
            Some(range) => self.iterate::<Message>(range.min.into(), range.max.into()),
        }
    }

    fn get_by_height(&self, h: Height) -> Box<dyn Iterator<Item = Message>> {
        let key = HeightKey::from(h);
        self.iterate(key, key)
    }

    fn get_only_by_height(&self, h: Height) -> Result<Message, OnlyError> {
        let mut as_vec: Vec<Message> = self.get_by_height(h).collect();
        match as_vec.len() {
            0 => Err(OnlyError::NoneAvailable),
            1 => Ok(as_vec.remove(0)),
            _ => Err(OnlyError::MultipleValues),
        }
    }

    fn get_by_height_range(&self, range: HeightRange) -> Box<dyn Iterator<Item = Message>> {
        match <dyn HeightIndexedPool<Message>>::height_range(self) {
            None => Box::new(std::iter::empty()),
            Some(bounds) => self.iterate::<Message>(
                HeightKey::from(range.min.max(bounds.min)),
                HeightKey::from(range.max.min(bounds.max)),
            ),
        }
    }

    fn get_highest_iter(&self) -> Box<dyn Iterator<Item = Message>> {
        match <dyn HeightIndexedPool<Message>>::max_height(self) {
            Some(height) => self.get_by_height(height),
            None => Box::new(std::iter::empty()),
        }
    }

    fn get_highest(&self) -> Result<Message, OnlyError> {
        let mut as_vec: Vec<Message> = self.get_highest_iter().collect();
        match as_vec.len() {
            0 => Err(OnlyError::NoneAvailable),
            1 => Ok(as_vec.remove(0)),
            _ => Err(OnlyError::MultipleValues),
        }
    }
}

///////////////////////////// Consensus Pool /////////////////////////////

const RANDOM_BEACON_KEY: TypeKey = TypeKey::new("RB");
const FINALIZATION_KEY: TypeKey = TypeKey::new("FZ");
const NOTARIZATION_KEY: TypeKey = TypeKey::new("NZ");
const BLOCK_PROPOSAL_KEY: TypeKey = TypeKey::new("BP");
const BLOCK_PAYLOAD_KEY: TypeKey = TypeKey::new("PL");
const RANDOM_BEACON_SHARE_KEY: TypeKey = TypeKey::new("RBS");
const NOTARIZATION_SHARE_KEY: TypeKey = TypeKey::new("NZS");
const FINALIZATION_SHARE_KEY: TypeKey = TypeKey::new("FZS");
const RANDOM_TAPE_KEY: TypeKey = TypeKey::new("RT");
const RANDOM_TAPE_SHARE_KEY: TypeKey = TypeKey::new("RTS");
const CATCH_UP_PACKAGE_KEY: TypeKey = TypeKey::new("CUP");
const CATCH_UP_PACKAGE_SHARE_KEY: TypeKey = TypeKey::new("CUS");

const CONSENSUS_KEYS: [TypeKey; 12] = [
    RANDOM_BEACON_KEY,
    FINALIZATION_KEY,
    NOTARIZATION_KEY,
    BLOCK_PROPOSAL_KEY,
    BLOCK_PAYLOAD_KEY,
    RANDOM_BEACON_SHARE_KEY,
    NOTARIZATION_SHARE_KEY,
    FINALIZATION_SHARE_KEY,
    RANDOM_TAPE_KEY,
    RANDOM_TAPE_SHARE_KEY,
    CATCH_UP_PACKAGE_KEY,
    CATCH_UP_PACKAGE_SHARE_KEY,
];

const CONSENSUS_SHARE_KEYS: [TypeKey; 2] = [NOTARIZATION_SHARE_KEY, FINALIZATION_SHARE_KEY];

impl HasTypeKey for RandomBeacon {
    fn type_key() -> TypeKey {
        RANDOM_BEACON_KEY
    }
}

impl HasTypeKey for Notarization {
    fn type_key() -> TypeKey {
        NOTARIZATION_KEY
    }
}

impl HasTypeKey for Finalization {
    fn type_key() -> TypeKey {
        FINALIZATION_KEY
    }
}

impl HasTypeKey for BlockProposal {
    fn type_key() -> TypeKey {
        BLOCK_PROPOSAL_KEY
    }
}

impl HasTypeKey for RandomBeaconShare {
    fn type_key() -> TypeKey {
        RANDOM_BEACON_SHARE_KEY
    }
}

impl HasTypeKey for NotarizationShare {
    fn type_key() -> TypeKey {
        NOTARIZATION_SHARE_KEY
    }
}

impl HasTypeKey for FinalizationShare {
    fn type_key() -> TypeKey {
        FINALIZATION_SHARE_KEY
    }
}

impl HasTypeKey for RandomTape {
    fn type_key() -> TypeKey {
        RANDOM_TAPE_KEY
    }
}

impl HasTypeKey for RandomTapeShare {
    fn type_key() -> TypeKey {
        RANDOM_TAPE_SHARE_KEY
    }
}

impl HasTypeKey for CatchUpPackage {
    fn type_key() -> TypeKey {
        CATCH_UP_PACKAGE_KEY
    }
}

impl HasTypeKey for CatchUpPackageShare {
    fn type_key() -> TypeKey {
        CATCH_UP_PACKAGE_SHARE_KEY
    }
}

impl From<ConsensusMessageId> for ArtifactKey {
    fn from(msg_id: ConsensusMessageId) -> Self {
        let type_key = match msg_id.hash {
            ConsensusMessageHash::RandomBeacon(_) => RANDOM_BEACON_KEY,
            ConsensusMessageHash::Finalization(_) => FINALIZATION_KEY,
            ConsensusMessageHash::Notarization(_) => NOTARIZATION_KEY,
            ConsensusMessageHash::BlockProposal(_) => BLOCK_PROPOSAL_KEY,
            ConsensusMessageHash::RandomBeaconShare(_) => RANDOM_BEACON_SHARE_KEY,
            ConsensusMessageHash::NotarizationShare(_) => NOTARIZATION_SHARE_KEY,
            ConsensusMessageHash::FinalizationShare(_) => FINALIZATION_SHARE_KEY,
            ConsensusMessageHash::RandomTape(_) => RANDOM_TAPE_KEY,
            ConsensusMessageHash::RandomTapeShare(_) => RANDOM_TAPE_SHARE_KEY,
            ConsensusMessageHash::CatchUpPackage(_) => CATCH_UP_PACKAGE_KEY,
            ConsensusMessageHash::CatchUpPackageShare(_) => CATCH_UP_PACKAGE_SHARE_KEY,
        };
        Self {
            type_key,
            height_key: HeightKey::from(msg_id.height),
            id_key: IdKey::from((msg_id.height, msg_id.hash.digest())),
        }
    }
}

impl TryFrom<ArtifactKey> for ConsensusMessageId {
    type Error = String;
    fn try_from(key: ArtifactKey) -> Result<Self, Self::Error> {
        let h = key.id_key.hash();
        let hash = match key.type_key {
            RANDOM_BEACON_KEY => ConsensusMessageHash::RandomBeacon(h.into()),
            FINALIZATION_KEY => ConsensusMessageHash::Finalization(h.into()),
            NOTARIZATION_KEY => ConsensusMessageHash::Notarization(h.into()),
            BLOCK_PROPOSAL_KEY => ConsensusMessageHash::BlockProposal(h.into()),
            RANDOM_BEACON_SHARE_KEY => ConsensusMessageHash::RandomBeaconShare(h.into()),
            NOTARIZATION_SHARE_KEY => ConsensusMessageHash::NotarizationShare(h.into()),
            FINALIZATION_SHARE_KEY => ConsensusMessageHash::FinalizationShare(h.into()),
            RANDOM_TAPE_KEY => ConsensusMessageHash::RandomTape(h.into()),
            RANDOM_TAPE_SHARE_KEY => ConsensusMessageHash::RandomTapeShare(h.into()),
            CATCH_UP_PACKAGE_KEY => ConsensusMessageHash::CatchUpPackage(h.into()),
            CATCH_UP_PACKAGE_SHARE_KEY => ConsensusMessageHash::CatchUpPackageShare(h.into()),
            BLOCK_PAYLOAD_KEY => {
                return Err("Block payloads do not have a ConsensusMessageId".into())
            }
            other => {
                return Err(format!(
                    "{:?} is not a valid ConsensusMessage TypeKey.",
                    other
                ))
            }
        };
        Ok(ConsensusMessageId {
            hash,
            height: key.id_key.height(),
        })
    }
}

impl PoolArtifact for ConsensusMessage {
    type ObjectType = ValidatedConsensusArtifact;
    type Id = ConsensusMessageId;

    fn type_keys() -> &'static [TypeKey] {
        &CONSENSUS_KEYS
    }

    fn save(
        key: &IdKey,
        mut value: Self::ObjectType,
        artifacts: Database,
        tx: &mut RwTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<()> {
        // special handling for block proposal & its payload
        if let ConsensusMessage::BlockProposal(mut proposal) = value.msg {
            // store block payload separately
            let block = proposal.content.as_mut();
            let payload_hash = block.payload.get_hash().clone();
            let payload = block.payload.as_ref();
            let start_height = payload.dkg_interval_start_height();
            let payload_type = payload.payload_type();
            {
                let payload_key = IdKey::from((block.height(), payload_hash.get_ref()));
                let bytes = log_err!(
                    bincode::serialize::<BlockPayload>(payload),
                    log,
                    "ConsensusArtifact::save serialize BlockPayload"
                )
                .ok_or(lmdb::Error::Panic)?;
                tx.put(artifacts, &payload_key, &bytes, WriteFlags::empty())?;
            }
            // replace block payload with an empty one
            block.payload = Payload::new_with(
                payload_hash,
                payload_type,
                // A dummy payload. Note that during deserialization, this dummy is
                // used to determine the payload type. So it's important that the
                // dummy has the SAME payload type as the real payload.
                Box::new(move || match payload_type {
                    PayloadType::Summary => (dkg::Summary::default(), None).into(),
                    PayloadType::Data => (
                        BatchPayload::default(),
                        dkg::Dealings::new_empty(start_height),
                        None,
                    )
                        .into(),
                }),
            );
            value.msg = proposal.into_message();
        }
        let bytes = pb::ValidatedConsensusArtifact::from(&value).encode_to_vec();
        tx.put(artifacts, &key, &bytes, WriteFlags::empty())
    }

    fn load_as<T: TryFrom<Self>>(
        key: &IdKey,
        db_env: Arc<Environment>,
        artifacts: Database,
        tx: &RoTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<T>
    where
        <T as TryFrom<Self>>::Error: Debug,
    {
        let bytes = tx.get(artifacts, &key)?;
        let protobuf = log_err!(
            pb::ValidatedConsensusArtifact::decode(bytes),
            log,
            "ConsensusArtifact::load_as protobuf decoding"
        )
        .ok_or(lmdb::Error::Panic)?;
        let artifact: ValidatedConsensusArtifact = log_err!(
            protobuf.try_into(),
            log,
            "ConsensusArtifact::load_as protobuf conversion"
        )
        .ok_or(lmdb::Error::Panic)?;

        let msg = match artifact.msg {
            ConsensusMessage::BlockProposal(mut proposal) => {
                // Lazy loading of block proposal and its payload
                let block = proposal.content.as_mut();
                let payload_hash = block.payload.get_hash();
                let payload_key = IdKey::from((block.height(), payload_hash.get_ref()));
                let log_clone = log.clone();
                block.payload = Payload::new_with(
                    payload_hash.clone(),
                    block.payload.payload_type(),
                    Box::new(move || {
                        log_err!(
                            load_block_payload(db_env, artifacts, &payload_key, &log_clone),
                            log_clone,
                            "ConsensusArtifact::load_as load_block_payload"
                        )
                        .unwrap()
                    }),
                );
                proposal.into_message()
            }
            consensus_message => consensus_message,
        };

        log_err!(
            T::try_from(msg),
            log,
            "ConsensusArtifact::load_as conversion"
        )
        .ok_or(lmdb::Error::Panic)
    }
}

/// Block payloads are loaded separately on demand.
fn load_block_payload(
    db_env: Arc<Environment>,
    artifacts: Database,
    payload_key: &IdKey,
    log: &ReplicaLogger,
) -> lmdb::Result<BlockPayload> {
    let tx = db_env.begin_ro_txn()?;
    let bytes = tx.get(artifacts, &payload_key)?;
    bincode::deserialize::<BlockPayload>(bytes).map_err(|err| {
        error!(log, "Error deserializing block payload: {:?}", err);
        lmdb::Error::Panic
    })
}

impl PersistentHeightIndexedPool<ConsensusMessage> {
    pub fn new_consensus_pool(
        config: LMDBConfig,
        read_only: bool,
        log: ReplicaLogger,
    ) -> PersistentHeightIndexedPool<ConsensusMessage> {
        let mut path = config.persistent_pool_validated_persistent_db_path;
        path.push("consensus");
        std::fs::create_dir_all(path.as_path()).ok();
        PersistentHeightIndexedPool::new(path.as_path(), read_only, log)
    }

    fn tx_mutate(
        &mut self,
        ops: PoolSectionOps<ValidatedConsensusArtifact>,
    ) -> lmdb::Result<Vec<ConsensusMessageId>> {
        let mut tx = self.db_env.begin_rw_txn()?;
        let mut purged = Vec::new();
        for op in ops.ops {
            match op {
                PoolSectionOp::Insert(artifact) => {
                    let msg_id = artifact.msg.get_id();
                    let key = ArtifactKey::from(msg_id);
                    // Ignore KeyExist
                    match self.tx_insert(&mut tx, &key, artifact) {
                        Err(lmdb::Error::KeyExist) => Ok(()),
                        result => result,
                    }?
                }
                PoolSectionOp::Remove(msg_id) => {
                    let key = ArtifactKey::from(msg_id.clone());
                    // Note: We do not remove block payloads here, but leave it to purging.
                    self.tx_remove(&mut tx, &key)?;
                    purged.push(msg_id);
                }
                PoolSectionOp::PurgeBelow(height) => {
                    let height_key = HeightKey::from(height);
                    purged.extend(
                        self.tx_purge_below(&mut tx, height_key)?
                            .into_iter()
                            .map(ConsensusMessageId::try_from)
                            .flat_map(|r| {
                                log_err!(r, self.log, "ConsensusMessage::tx_mutate PurgeBelow")
                            }),
                    );
                }
                PoolSectionOp::PurgeSharesBelow(height) => {
                    let height_key = HeightKey::from(height);
                    for type_key in CONSENSUS_SHARE_KEYS {
                        purged.extend(
                            self.tx_purge_type_below(&mut tx, type_key, height_key)?
                                .into_iter()
                                .map(ConsensusMessageId::try_from)
                                .flat_map(|r| {
                                    log_err!(
                                        r,
                                        self.log,
                                        "ConsensusMessage::tx_mutate PurgeSharesBelow"
                                    )
                                }),
                        );
                    }
                }
            }
        }
        tx.commit()?;
        Ok(purged)
    }
}

impl crate::consensus_pool::MutablePoolSection<ValidatedConsensusArtifact>
    for PersistentHeightIndexedPool<ConsensusMessage>
{
    fn mutate(
        &mut self,
        ops: PoolSectionOps<ValidatedConsensusArtifact>,
    ) -> Vec<ConsensusMessageId> {
        match self.tx_mutate(ops) {
            Ok(purged) => purged,
            err => {
                log_err!(err, self.log, "ConsensusArtifact::mutate");
                Vec::new()
            }
        }
    }

    fn pool_section(&self) -> &dyn PoolSection<ValidatedConsensusArtifact> {
        self
    }
}

impl PoolSection<ValidatedConsensusArtifact> for PersistentHeightIndexedPool<ConsensusMessage> {
    fn contains(&self, msg_id: &ConsensusMessageId) -> bool {
        if let Some(tx) = log_err!(self.db_env.begin_ro_txn(), self.log, "begin_ro_txn") {
            let key = IdKey::from(msg_id);
            log_err_except!(
                tx.get(self.artifacts, &key),
                self.log,
                lmdb::Error::NotFound,
                format!("contains {:?}", msg_id)
            )
            .is_some()
        } else {
            false
        }
    }

    fn get(&self, msg_id: &ConsensusMessageId) -> Option<ConsensusMessage> {
        let tx = log_err!(self.db_env.begin_ro_txn(), self.log, "begin_ro_txn")?;
        let key = IdKey::from(msg_id);
        log_err_except!(
            ConsensusMessage::load_as::<ConsensusMessage>(
                &key,
                self.db_env.clone(),
                self.artifacts,
                &tx,
                &self.log
            ),
            self.log,
            lmdb::Error::NotFound,
            format!("get {:?}", msg_id)
        )
    }

    fn get_timestamp(&self, msg_id: &ConsensusMessageId) -> Option<Time> {
        let tx = log_err!(self.db_env.begin_ro_txn(), self.log, "begin_ro_txn")?;
        let key = IdKey::from(msg_id);
        let bytes = log_err_except!(
            tx.get(self.artifacts, &key),
            self.log,
            lmdb::Error::NotFound,
            format!("get_timestamp get {:?}", msg_id)
        )?;
        log_err!(
            pb::ValidatedConsensusArtifact::decode(bytes),
            self.log,
            "get_timestamp deserialize"
        )
        .map(|x| Time::from_nanos_since_unix_epoch(x.timestamp))
    }

    fn random_beacon(&self) -> &dyn HeightIndexedPool<RandomBeacon> {
        self
    }

    fn block_proposal(&self) -> &dyn HeightIndexedPool<BlockProposal> {
        self
    }

    fn notarization(&self) -> &dyn HeightIndexedPool<Notarization> {
        self
    }

    fn finalization(&self) -> &dyn HeightIndexedPool<Finalization> {
        self
    }

    fn random_beacon_share(&self) -> &dyn HeightIndexedPool<RandomBeaconShare> {
        self
    }

    fn notarization_share(&self) -> &dyn HeightIndexedPool<NotarizationShare> {
        self
    }

    fn finalization_share(&self) -> &dyn HeightIndexedPool<FinalizationShare> {
        self
    }

    fn random_tape(&self) -> &dyn HeightIndexedPool<RandomTape> {
        self
    }

    fn random_tape_share(&self) -> &dyn HeightIndexedPool<RandomTapeShare> {
        self
    }

    fn catch_up_package(&self) -> &dyn HeightIndexedPool<CatchUpPackage> {
        self
    }

    fn catch_up_package_share(&self) -> &dyn HeightIndexedPool<CatchUpPackageShare> {
        self
    }

    fn highest_catch_up_package_proto(&self) -> pb::CatchUpPackage {
        let h = self
            .catch_up_package()
            .max_height()
            .expect("There should always be a CUP in the pool.");
        let key = HeightKey::from(h);
        let index_db = self.get_index_db(&CatchUpPackage::type_key());
        let log = self.log.clone();
        let artifacts = self.artifacts;
        LMDBIterator::new(
            self.db_env.clone(),
            index_db,
            key,
            key,
            move |tx: &RoTransaction<'_>, key: &[u8]| {
                let bytes = tx.get(artifacts, &key)?;
                let artifact = log_err!(
                    pb::ValidatedConsensusArtifact::decode(bytes),
                    log,
                    "CatchUpPackage protobuf deserialize"
                )
                .ok_or(lmdb::Error::Panic)?;
                match artifact.msg {
                    Some(pb::ConsensusMessage {
                        msg: Some(pb::consensus_message::Msg::Cup(cup_proto)),
                    }) => Ok(cup_proto),
                    Some(_) => panic!("unexpected artifact type when deserializing CUP"),
                    None => panic!("No consensus message found"),
                }
            },
            self.log.clone(),
        )
        .next()
        .unwrap_or_else(|| {
            panic!(
                "This should be impossible since we found a max height at {:?}",
                h
            )
        })
    }

    /// Number of artifacts in the DB.
    fn size(&self) -> u64 {
        if let Some(tx) = log_err!(self.db_env.begin_ro_txn(), &self.log, "begin_ro_txn") {
            if let Some(mut cursor) = log_err!(
                tx.open_ro_cursor(self.artifacts),
                &self.log,
                "open_ro_cursor"
            ) {
                return cursor.iter().count() as u64;
            }
        }
        0
    }
}

///////////////////////////// Certification Pool /////////////////////////////

const CERTIFICATION_KEY: TypeKey = TypeKey::new("CE");
const CERTIFICATION_SHARE_KEY: TypeKey = TypeKey::new("CES");

const CERTIFICATION_KEYS: [TypeKey; 2] = [CERTIFICATION_KEY, CERTIFICATION_SHARE_KEY];

impl HasTypeKey for Certification {
    fn type_key() -> TypeKey {
        CERTIFICATION_KEY
    }
}

impl HasTypeKey for CertificationShare {
    fn type_key() -> TypeKey {
        CERTIFICATION_SHARE_KEY
    }
}

impl TryFrom<ArtifactKey> for CertificationMessageId {
    type Error = String;
    fn try_from(key: ArtifactKey) -> Result<Self, Self::Error> {
        let h = key.id_key.hash();
        let hash = match key.type_key {
            CERTIFICATION_KEY => CertificationMessageHash::Certification(h.into()),
            CERTIFICATION_SHARE_KEY => CertificationMessageHash::CertificationShare(h.into()),
            other => {
                return Err(format!(
                    "{:?} is not a valid CertificationMessage TypeKey.",
                    other
                ))
            }
        };
        Ok(CertificationMessageId {
            hash,
            height: key.id_key.height(),
        })
    }
}

impl PoolArtifact for CertificationMessage {
    type ObjectType = CertificationMessage;
    type Id = CertificationMessageId;

    fn type_keys() -> &'static [TypeKey] {
        &CERTIFICATION_KEYS
    }

    fn save(
        key: &IdKey,
        value: Self::ObjectType,
        artifacts: Database,
        tx: &mut RwTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<()> {
        let bytes = log_err!(
            bincode::serialize::<Self::ObjectType>(&value),
            log,
            "CertificationArtifact::save serialize"
        )
        .ok_or(lmdb::Error::Panic)?;
        tx.put(artifacts, &key, &bytes, WriteFlags::empty())
    }

    fn load_as<T: TryFrom<Self>>(
        key: &IdKey,
        _db_env: Arc<Environment>,
        artifacts: Database,
        tx: &RoTransaction,
        log: &ReplicaLogger,
    ) -> lmdb::Result<T> {
        let bytes = tx.get(artifacts, &key)?;
        let msg = log_err!(
            bincode::deserialize::<Self::ObjectType>(bytes),
            log,
            "CertificationArtifact::load_as deserialize"
        )
        .ok_or(lmdb::Error::Panic)?;
        log_err!(
            msg.try_into().map_err(|_| ()),
            log,
            "CertificationArtifact::load_as casting"
        )
        .ok_or(lmdb::Error::Panic)
    }
}

impl PersistentHeightIndexedPool<CertificationMessage> {
    pub fn new_certification_pool(
        config: LMDBConfig,
        read_only: bool,
        log: ReplicaLogger,
    ) -> PersistentHeightIndexedPool<CertificationMessage> {
        let mut path = config.persistent_pool_validated_persistent_db_path;
        path.push("certification");
        std::fs::create_dir_all(path.as_path()).ok();
        PersistentHeightIndexedPool::new(path.as_path(), read_only, log)
    }

    fn insert_message<T: HasTypeKey + Into<CertificationMessage> + CryptoHashable + HasHeight>(
        &self,
        hash: CryptoHashOf<T>,
        value: T,
    ) -> lmdb::Result<()> {
        let key = ArtifactKey {
            type_key: T::type_key(),
            id_key: IdKey::from((value.height(), hash.get_ref())),
            height_key: HeightKey::from(value.height()),
        };
        let mut tx = self.db_env.begin_rw_txn()?;
        self.tx_insert(&mut tx, &key, value.into())?;
        tx.commit()
    }

    fn purge_below_height(&self, height: Height) -> lmdb::Result<Vec<CertificationMessageId>> {
        let mut tx = self.db_env.begin_rw_txn()?;
        let purged = self
            .tx_purge_below(&mut tx, HeightKey::from(height))?
            .into_iter()
            .map(CertificationMessageId::try_from)
            .flat_map(|r| log_err!(r, self.log, "CertificationMessage::purge_below_height"))
            .collect();
        tx.commit()?;
        Ok(purged)
    }
}

impl crate::certification_pool::MutablePoolSection
    for PersistentHeightIndexedPool<CertificationMessage>
{
    fn insert(&self, message: CertificationMessage) {
        match message {
            CertificationMessage::Certification(value) => log_err!(
                self.insert_message(ic_types::crypto::crypto_hash(&value), value),
                self.log,
                "CertificationMessage::Certification::insert"
            ),
            CertificationMessage::CertificationShare(value) => log_err!(
                self.insert_message(ic_types::crypto::crypto_hash(&value), value),
                self.log,
                "CertificationMessage::CertificationShare::insert"
            ),
        };
    }

    fn purge_below(&self, height: Height) -> Vec<CertificationMessageId> {
        match self.purge_below_height(height) {
            Ok(purged) => purged,
            err => {
                log_err!(err, self.log, "CertificationArtifact::purge_below");
                Vec::new()
            }
        }
    }

    fn certifications(&self) -> &dyn HeightIndexedPool<Certification> {
        self
    }

    fn certification_shares(&self) -> &dyn HeightIndexedPool<CertificationShare> {
        self
    }
}

///////////////////////////// ECDSA Pool /////////////////////////////

impl From<EcdsaMessageId> for IdKey {
    fn from(msg_id: EcdsaMessageId) -> IdKey {
        let prefix = msg_id.prefix();
        let mut bytes = vec![];
        bytes.extend_from_slice(&u64::to_be_bytes(prefix.group_tag()));
        bytes.extend_from_slice(&u64::to_be_bytes(prefix.meta_hash()));
        bytes.extend_from_slice(&msg_id.hash().0);
        IdKey(bytes)
    }
}

impl From<&EcdsaPrefix> for IdKey {
    fn from(prefix: &EcdsaPrefix) -> IdKey {
        let mut bytes = vec![];
        bytes.extend_from_slice(&u64::to_be_bytes(prefix.group_tag()));
        bytes.extend_from_slice(&u64::to_be_bytes(prefix.meta_hash()));
        IdKey(bytes)
    }
}

fn deser_ecdsa_message_id(message_type: EcdsaMessageType, id_key: IdKey) -> EcdsaMessageId {
    let mut group_tag_bytes = [0; 8];
    group_tag_bytes.copy_from_slice(&id_key.0[0..8]);

    let mut meta_hash_bytes = [0; 8];
    meta_hash_bytes.copy_from_slice(&id_key.0[8..16]);

    let crypto_hash_bytes: &[u8] = &id_key.0[16..];

    (
        message_type,
        EcdsaPrefix::new_with_meta_hash(
            u64::from_be_bytes(group_tag_bytes),
            u64::from_be_bytes(meta_hash_bytes),
        ),
        CryptoHash(crypto_hash_bytes.to_vec()),
    )
        .into()
}

/// The per-message type DB
struct EcdsaMessageDb {
    db_env: Arc<Environment>,
    db: Database,
    object_type: EcdsaMessageType,
    metrics: EcdsaPoolMetrics,
    log: ReplicaLogger,
}

impl EcdsaMessageDb {
    fn new(
        db_env: Arc<Environment>,
        db: Database,
        object_type: EcdsaMessageType,
        metrics: EcdsaPoolMetrics,
        log: ReplicaLogger,
    ) -> Self {
        Self {
            db_env,
            db,
            object_type,
            metrics,
            log,
        }
    }

    /// Adds the serialized <key, vale> to be added to the transaction. Returns true on success,
    /// false otherwise.
    fn insert_txn(&self, message: EcdsaMessage, tx: &mut RwTransaction) -> bool {
        assert_eq!(EcdsaMessageType::from(&message), self.object_type);
        let key = IdKey::from(EcdsaArtifactId::from(&message));
        let bytes = match bincode::serialize::<EcdsaMessage>(&message) {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(
                    self.log,
                    "EcdsaMessageDb::insert_txn(): serialize(): {:?}/{:?}", key, err
                );
                self.metrics.persistence_error("insert_serialize");
                return false;
            }
        };

        if let Err(err) = tx.put(self.db, &key, &bytes, WriteFlags::empty()) {
            error!(
                self.log,
                "EcdsaMessageDb::insert_txn(): tx.put(): {:?}/{:?}", key, err
            );
            self.metrics.persistence_error("insert_tx_put");
            return false;
        }

        true
    }

    fn get_object(&self, id: &EcdsaMessageId) -> Option<EcdsaMessage> {
        let key = IdKey::from(id.clone());
        let tx = match self.db_env.begin_ro_txn() {
            Ok(tx) => tx,
            Err(err) => {
                error!(
                    self.log,
                    "EcdsaMessageDb::get(): begin_ro_txn(): {:?}/{:?}", key, err
                );
                self.metrics.persistence_error("get_begin_ro_txn");
                return None;
            }
        };

        let bytes = match tx.get(self.db, &key) {
            Ok(bytes) => bytes,
            Err(lmdb::Error::NotFound) => return None,
            Err(err) => {
                error!(
                    self.log,
                    "EcdsaMessageDb::get(): tx.get(): {:?}/{:?}", key, err
                );
                self.metrics.persistence_error("get_tx_get");
                return None;
            }
        };

        match bincode::deserialize::<EcdsaMessage>(bytes) {
            Ok(msg) => Some(msg),
            Err(err) => {
                error!(
                    self.log,
                    "EcdsaMessageDb::get(): deserialize(): {:?}/{:?}", key, err
                );
                self.metrics.persistence_error("get_deserialize");
                None
            }
        }
    }

    /// Adds the serialized <key> to be removed to the transaction. Returns true on success,
    /// false otherwise.
    fn remove_txn(&self, id: &EcdsaMessageId, tx: &mut RwTransaction) -> bool {
        let key = IdKey::from(id.clone());
        if let Err(err) = tx.del(self.db, &key, None) {
            error!(
                self.log,
                "EcdsaMessageDb::remove_txn(): tx.del(): {:?}/{:?}", key, err
            );
            self.metrics.persistence_error("remove_tx_del");
            return false;
        }
        true
    }

    fn iter<T: TryFrom<EcdsaMessage>>(
        &self,
        prefix: Option<EcdsaPrefixOf<T>>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, T)> + '_>
    where
        <T as TryFrom<EcdsaMessage>>::Error: Debug,
    {
        let message_type = self.object_type;
        let log = self.log.clone();
        let prefix_cl = prefix.as_ref().map(|p| p.as_ref().clone());
        let deserialize_fn = move |key: &[u8], bytes: &[u8]| {
            // Convert key bytes to EcdsaMessageId
            let mut key_bytes = Vec::<u8>::new();
            key_bytes.extend_from_slice(key);
            let id_key = IdKey(key_bytes);
            let id = deser_ecdsa_message_id(message_type, id_key);

            // Stop iterating if we hit a different prefix.
            if let Some(prefix) = &prefix_cl {
                if id.prefix() != *prefix {
                    return None;
                }
            }

            // Deserialize value bytes and convert to inner type
            let message = match bincode::deserialize::<EcdsaMessage>(bytes) {
                Ok(message) => message,
                Err(err) => {
                    error!(
                        log,
                        "EcdsaMessageDb::iter(): deserialize() failed: {:?}/{:?}/{}/{}",
                        id,
                        err,
                        key.len(),
                        bytes.len()
                    );
                    return None;
                }
            };

            match T::try_from(message) {
                Ok(inner) => Some((id, inner)),
                Err(err) => {
                    error!(
                        log,
                        "EcdsaMessageDb::iter(): failed to convert to inner type: {:?}/{:?}/{}/{}",
                        id,
                        err,
                        key.len(),
                        bytes.len()
                    );
                    None
                }
            }
        };

        Box::new(LMDBEcdsaIterator::new(
            self.db_env.clone(),
            self.db,
            deserialize_fn,
            prefix.map(|p| p.as_ref().into()),
            self.log.clone(),
        ))
    }
}

/// The PersistentEcdsaPoolSection is just a collection of per-message type
/// backend DBs. The main role is to route the operations to the appropriate
/// backend DB.
pub(crate) struct PersistentEcdsaPoolSection {
    // Per message type data base
    db_env: Arc<Environment>,
    message_dbs: Vec<(EcdsaMessageType, EcdsaMessageDb)>,
    metrics: EcdsaPoolMetrics,
    log: ReplicaLogger,
}

impl PersistentEcdsaPoolSection {
    pub(crate) fn new_ecdsa_pool(
        config: LMDBConfig,
        read_only: bool,
        log: ReplicaLogger,
        metrics_registry: MetricsRegistry,
        pool: &str,
        pool_type: &str,
    ) -> Self {
        let mut type_keys = Vec::new();
        for message_type in EcdsaMessageType::iter() {
            type_keys.push((message_type, Self::get_type_key(message_type)));
        }

        let mut path = config.persistent_pool_validated_persistent_db_path;
        path.push("ecdsa");
        if let Err(err) = std::fs::create_dir_all(path.as_path()) {
            panic!("Error creating ECDSA dir {:?}: {:?}", path, err)
        }
        let db_env = Arc::new(create_db_env(
            path.as_path(),
            read_only,
            type_keys.len() as c_uint,
        ));

        let mut message_dbs = Vec::new();
        let metrics = EcdsaPoolMetrics::new(metrics_registry, pool, pool_type);
        for (message_type, type_key) in &type_keys {
            let db = if read_only {
                db_env.open_db(Some(type_key.name)).unwrap_or_else(|err| {
                    panic!("Error opening ECDSA db {}: {:?}", type_key.name, err)
                })
            } else {
                db_env
                    .create_db(Some(type_key.name), DatabaseFlags::empty())
                    .unwrap_or_else(|err| {
                        panic!("Error creating ECDSA db {}: {:?}", type_key.name, err)
                    })
            };
            message_dbs.push((
                *message_type,
                EcdsaMessageDb::new(
                    db_env.clone(),
                    db,
                    *message_type,
                    metrics.clone(),
                    log.clone(),
                ),
            ));
        }

        info!(
            log,
            "PersistentEcdsaPoolSection::new_ecdsa_pool(): num_dbs = {}",
            type_keys.len()
        );

        Self {
            db_env,
            message_dbs,
            metrics,
            log,
        }
    }

    fn get_message_db(&self, message_type: EcdsaMessageType) -> &EcdsaMessageDb {
        self.message_dbs
            .iter()
            .find(|(db_type, _)| *db_type == message_type)
            .map(|(_, db)| db)
            .unwrap()
    }

    fn get_type_key(message_type: EcdsaMessageType) -> TypeKey {
        match message_type {
            EcdsaMessageType::Dealing => TypeKey::new("ECD"),
            EcdsaMessageType::DealingSupport => TypeKey::new("ECS"),
            EcdsaMessageType::SigShare => TypeKey::new("ECI"),
            EcdsaMessageType::Complaint => TypeKey::new("ECC"),
            EcdsaMessageType::Opening => TypeKey::new("ECO"),
        }
    }
}

impl EcdsaPoolSection for PersistentEcdsaPoolSection {
    fn contains(&self, msg_id: &EcdsaMessageId) -> bool {
        self.get_message_db(EcdsaMessageType::from(msg_id))
            .get_object(msg_id)
            .is_some()
    }

    fn get(&self, msg_id: &EcdsaMessageId) -> Option<EcdsaMessage> {
        self.get_message_db(EcdsaMessageType::from(msg_id))
            .get_object(msg_id)
    }

    fn signed_dealings(
        &self,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, SignedIDkgDealing)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Dealing);
        message_db.iter(None)
    }

    fn signed_dealings_by_prefix(
        &self,
        prefix: EcdsaPrefixOf<SignedIDkgDealing>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, SignedIDkgDealing)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Dealing);
        message_db.iter(Some(prefix))
    }

    fn dealing_support(
        &self,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, IDkgDealingSupport)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::DealingSupport);
        message_db.iter(None)
    }

    fn dealing_support_by_prefix(
        &self,
        prefix: EcdsaPrefixOf<IDkgDealingSupport>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, IDkgDealingSupport)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::DealingSupport);
        message_db.iter(Some(prefix))
    }

    fn signature_shares(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaSigShare)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::SigShare);
        message_db.iter(None)
    }

    fn signature_shares_by_prefix(
        &self,
        prefix: EcdsaPrefixOf<EcdsaSigShare>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaSigShare)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::SigShare);
        message_db.iter(Some(prefix))
    }

    fn complaints(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaComplaint)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Complaint);
        message_db.iter(None)
    }

    fn complaints_by_prefix(
        &self,
        prefix: EcdsaPrefixOf<EcdsaComplaint>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaComplaint)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Complaint);
        message_db.iter(Some(prefix))
    }

    fn openings(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaOpening)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Opening);
        message_db.iter(None)
    }

    fn openings_by_prefix(
        &self,
        prefix: EcdsaPrefixOf<EcdsaOpening>,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaOpening)> + '_> {
        let message_db = self.get_message_db(EcdsaMessageType::Opening);
        message_db.iter(Some(prefix))
    }
}

impl MutableEcdsaPoolSection for PersistentEcdsaPoolSection {
    fn mutate(&mut self, ops: EcdsaPoolSectionOps) {
        if ops.ops.is_empty() {
            return;
        }

        let mut tx = match self.db_env.begin_rw_txn() {
            Ok(tx) => tx,
            Err(err) => {
                error!(
                    self.log,
                    "MutableEcdsaPoolSection::mutate(): begin_rw_txn(): {:?}", err
                );
                self.metrics.persistence_error("begin_rw_txn");
                return;
            }
        };

        for op in ops.ops {
            match op {
                EcdsaPoolSectionOp::Insert(message) => {
                    let message_type = EcdsaMessageType::from(&message);
                    let db = self.get_message_db(message_type);
                    if !db.insert_txn(message, &mut tx) {
                        return;
                    }
                    self.metrics.observe_insert(message_type.as_str());
                }
                EcdsaPoolSectionOp::Remove(id) => {
                    let message_type = EcdsaMessageType::from(&id);
                    let db = self.get_message_db(message_type);
                    if !db.remove_txn(&id, &mut tx) {
                        return;
                    }
                    self.metrics.observe_remove(message_type.as_str())
                }
            }
        }

        match tx.commit() {
            Ok(()) => (),
            Err(lmdb::Error::NotFound) => {
                self.metrics.persistence_error("tx_commit_not_found");
            }
            Err(err) => {
                error!(
                    self.log,
                    "MutableEcdsaPoolSection::mutate(): tx.commit(): {:?}", err
                );
                self.metrics.persistence_error("tx_commit");
            }
        }
    }

    fn as_pool_section(&self) -> &dyn EcdsaPoolSection {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        consensus_pool::MutablePoolSection,
        test_utils::{
            fake_random_beacon, finalization_share_ops, random_beacon_ops, PoolTestHelper,
        },
    };
    use ic_test_utilities_logger::with_test_replica_logger;
    use std::{panic, path::PathBuf};

    #[test]
    fn test_encode_decode_key() {
        let height = Height::from(10);
        let beacon = fake_random_beacon(height);
        let msg = ConsensusMessage::RandomBeacon(beacon);
        let hash = msg.get_cm_hash();
        let height_key = HeightKey::from(height);
        let id_key = IdKey::from((height, hash.digest()));
        assert_eq!(Height::from(height_key), height, "height does not match");
        assert_eq!(id_key.height(), height, "Height of IdKey does not match");
        assert_eq!(
            &id_key.hash(),
            hash.digest(),
            "Hash of IdKey does not match"
        );
    }

    // TODO: Remove this after it is no longer needed
    // Helper to run the persistence tests below.
    // It creates the config and logger that is passed to the instances and then
    // makes sure that the the databases are destroyed before the test fails.
    fn run_persistent_pool_test<T>(_test_name: &str, test: T)
    where
        T: FnOnce(LMDBConfig, ReplicaLogger) + panic::UnwindSafe,
    {
        with_test_replica_logger(|log| {
            ic_test_utilities::artifact_pool_config::with_test_lmdb_pool_config(|config| {
                let result = panic::catch_unwind(|| test(config.clone(), log));
                assert!(result.is_ok());
            })
        })
    }

    impl PoolTestHelper for LMDBConfig {
        type PersistentHeightIndexedPool = PersistentHeightIndexedPool<ConsensusMessage>;

        fn run_persistent_pool_test<T, R>(_test_name: &str, test: T) -> R
        where
            T: FnOnce(LMDBConfig, ReplicaLogger) -> R + panic::UnwindSafe,
        {
            with_test_replica_logger(|log| {
                ic_test_utilities::artifact_pool_config::with_test_lmdb_pool_config(|config| {
                    let result = panic::catch_unwind(|| test(config.clone(), log));
                    assert!(result.is_ok());
                    result.unwrap()
                })
            })
        }

        fn new_consensus_pool(self, log: ReplicaLogger) -> Self::PersistentHeightIndexedPool {
            PersistentHeightIndexedPool::new_consensus_pool(self, false, log)
        }

        fn persistent_pool_validated_persistent_db_path(&self) -> &PathBuf {
            &self.persistent_pool_validated_persistent_db_path
        }
    }

    #[test]
    fn test_as_pool_section() {
        crate::test_utils::test_as_pool_section::<LMDBConfig>()
    }

    #[test]
    fn test_as_height_indexed_pool() {
        crate::test_utils::test_as_height_indexed_pool::<LMDBConfig>()
    }

    #[test]
    fn test_block_proposal_and_payload_correspondence() {
        crate::test_utils::test_block_proposal_and_payload_correspondence::<LMDBConfig>()
    }

    #[test]
    fn test_iterating_while_inserting_doesnt_see_new_updates() {
        crate::test_utils::test_iterating_while_inserting_doesnt_see_new_updates::<LMDBConfig>()
    }

    #[test]
    fn test_iterator_can_outlive_the_pool() {
        crate::test_utils::test_iterator_can_outlive_the_pool::<LMDBConfig>()
    }

    #[test]
    fn test_persistent_pool_path_is_cleanedup_after_tests() {
        crate::test_utils::test_persistent_pool_path_is_cleanedup_after_tests::<LMDBConfig>()
    }

    #[test]
    fn test_purge_survives_reboot() {
        run_persistent_pool_test("test_purge_survives_reboot", |config, log| {
            // create a pool and purge at height 10
            let height10 = Height::from(10);
            {
                let mut pool = PersistentHeightIndexedPool::new_consensus_pool(
                    config.clone(),
                    false,
                    log.clone(),
                );
                // insert a few things
                let rb_ops = random_beacon_ops();
                pool.mutate(rb_ops.clone());
                let iter = pool.random_beacon().get_all();
                let msgs_from_pool = iter;
                assert_eq!(msgs_from_pool.count(), rb_ops.ops.len());
                // purge at height 10
                let mut purge_ops = PoolSectionOps::new();
                purge_ops.purge_below(height10);
                pool.mutate(purge_ops);
                assert_eq!(
                    pool.random_beacon().height_range().map(|r| r.min),
                    Some(height10)
                );
            }
            // create the same pool again, check if purge was persisted
            {
                let pool = PersistentHeightIndexedPool::new_consensus_pool(config, false, log);
                assert_eq!(
                    pool.random_beacon().height_range().map(|r| r.min),
                    Some(height10)
                );
            }
        });
    }

    #[test]
    fn test_purge_shares_survives_reboot() {
        run_persistent_pool_test("test_purge_shares_survives_reboot", |config, log| {
            // create a pool and purge at height 10
            let height10 = Height::from(10);
            {
                let mut pool = PersistentHeightIndexedPool::new_consensus_pool(
                    config.clone(),
                    false,
                    log.clone(),
                );
                // insert random beacons and finalization shares
                let fs_ops = finalization_share_ops();
                pool.mutate(fs_ops.clone());
                pool.mutate(random_beacon_ops());
                // min height should be less than 10
                assert!(pool.finalization_share().height_range().map(|r| r.min) < Some(height10));

                let iter = pool.finalization_share().get_all();
                let shares_from_pool = iter.count();
                assert_eq!(shares_from_pool, fs_ops.ops.len());
                assert_consistency(&pool);
                let iter = pool.random_beacon().get_all();
                let messages_from_pool = iter.count();

                // purge at height 10
                let mut purge_ops = PoolSectionOps::new();
                purge_ops.purge_shares_below(height10);
                pool.mutate(purge_ops);
                // min height should be 10
                assert_eq!(
                    pool.finalization_share().height_range().map(|r| r.min),
                    Some(height10)
                );
                // full beacon count should be unchanged
                assert_eq!(pool.random_beacon().get_all().count(), messages_from_pool);
                assert_consistency(&pool);
            }
            // create the same pool again, check if purge was persisted
            {
                let pool = PersistentHeightIndexedPool::new_consensus_pool(config, false, log);
                assert_eq!(
                    pool.finalization_share().height_range().map(|r| r.min),
                    Some(height10)
                );
                assert_consistency(&pool);
            }
        });
    }

    // Assert that entries in artifacts db are reflected by index db and vice versa.
    // Each entry should have a join partner when joining on IdKey.
    fn assert_consistency(pool: &PersistentHeightIndexedPool<ConsensusMessage>) {
        let tx = pool.db_env.begin_ro_txn().unwrap();
        // get all ids from all indices
        let mut ids_index = pool
            .indices
            .iter()
            .flat_map(|(_, db)| {
                let mut cursor = tx.open_ro_cursor(*db).unwrap();
                cursor
                    .iter()
                    .map(|res| {
                        let (_, id) = res.unwrap();
                        IdKey::from(id)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        ids_index.sort();

        // get all ids from artifacts db
        let ids_artifacts = {
            let mut cursor = tx.open_ro_cursor(pool.artifacts).unwrap();
            cursor
                .iter()
                .map(|res| {
                    let (id, _) = res.unwrap();
                    IdKey::from(id)
                })
                .collect::<Vec<_>>()
        };
        tx.commit().unwrap();

        // they should be equal
        assert_eq!(ids_index, ids_artifacts);
    }

    #[test]
    fn test_timestamp_survives_reboot() {
        crate::test_utils::test_timestamp_survives_reboot::<LMDBConfig>()
    }
}
