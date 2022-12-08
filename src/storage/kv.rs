use futures::SinkExt;
use kuska_ssb::feed::{Feed as MessageKVT, Message as MessageValue};
use serde::{Deserialize, Serialize};

use crate::broker::{BrokerEvent, ChBrokerSend, Destination};

/// Prefix for a key to the latest sequence number for a stored feed.
const PREFIX_LATEST_SEQ: u8 = 0u8;
/// Prefix for a key to a message KVT (Key Value Timestamp).
const PREFIX_MSG_KVT: u8 = 1u8;
/// Prefix for a key to a message value (the 'V' in KVT).
const PREFIX_MSG_VAL: u8 = 2u8;
/// Prefix for a key to a blob.
const PREFIX_BLOB: u8 = 3u8;
/// Prefix for a key to a peer.
const PREFIX_PEER: u8 = 4u8;

#[derive(Debug, Clone)]
pub enum StoKvEvent {
    IdChanged(String),
}

#[derive(Default)]
pub struct KvStorage {
    db: Option<sled::Db>,
    ch_broker: Option<ChBrokerSend>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobStatus {
    retrieved: bool,
    users: Vec<String>,
}

/// The public key (ID) of a peer and a message sequence number.
#[derive(Debug, Serialize, Deserialize)]
pub struct PubKeyAndSeqNum {
    pub_key: String,
    seq_num: u64,
}

#[derive(Debug)]
pub enum Error {
    InvalidSequence,
    Sled(sled::Error),
    // TODO: not sure about renaming this.
    Feed(kuska_ssb::feed::Error),
    Cbor(serde_cbor::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl From<sled::Error> for Error {
    fn from(err: sled::Error) -> Self {
        Error::Sled(err)
    }
}

impl From<kuska_ssb::feed::Error> for Error {
    fn from(err: kuska_ssb::feed::Error) -> Self {
        Error::Feed(err)
    }
}

impl From<serde_cbor::Error> for Error {
    fn from(err: serde_cbor::Error) -> Self {
        Error::Cbor(err)
    }
}

impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;

impl KvStorage {
    /// Open the key-value database using the given configuration and populate
    /// the instance of `KvStorage` with the database and message-passing
    /// sender.
    pub fn open(&mut self, config: sled::Config, ch_broker: ChBrokerSend) -> Result<()> {
        self.db = Some(config.open()?);
        self.ch_broker = Some(ch_broker);
        Ok(())
    }

    /// Generate a key for the latest sequence number of the feed authored by
    /// the given public key.
    fn key_latest_seq(user_id: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.push(PREFIX_LATEST_SEQ);
        key.extend_from_slice(user_id.as_bytes());
        key
    }

    /// Generate a key for a message KVT authored by the given public key and
    /// with the given message sequence number.
    fn key_msg_kvt(user_id: &str, msg_seq: u64) -> Vec<u8> {
        let mut key = Vec::new();
        key.push(PREFIX_MSG_KVT);
        key.extend_from_slice(&msg_seq.to_be_bytes()[..]);
        key.extend_from_slice(user_id.as_bytes());
        key
    }

    /// Generate a key for a message value with the given ID (reference).
    fn key_msg_val(msg_id: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.push(PREFIX_MSG_VAL);
        key.extend_from_slice(msg_id.as_bytes());
        key
    }

    /// Generate a key for a blob with the given ID (reference).
    fn key_blob(blob_id: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.push(PREFIX_BLOB);
        key.extend_from_slice(blob_id.as_bytes());
        key
    }

    /// Generate a key for a peer with the given public key.
    fn key_peer(user_id: &str) -> Vec<u8> {
        let mut key = Vec::new();
        key.push(PREFIX_PEER);
        key.extend_from_slice(user_id.as_bytes());
        key
    }

    /// Get the status of a blob with the given ID.
    pub fn get_blob(&self, blob_id: &str) -> Result<Option<BlobStatus>> {
        let db = self.db.as_ref().unwrap();
        if let Some(raw) = db.get(Self::key_blob(blob_id))? {
            Ok(serde_cbor::from_slice(&raw)?)
        } else {
            Ok(None)
        }
    }

    /// Set the status of a blob with the given ID.
    pub fn set_blob(&self, blob_id: &str, blob: &BlobStatus) -> Result<()> {
        let db = self.db.as_ref().unwrap();
        let raw = serde_cbor::to_vec(blob)?;
        db.insert(Self::key_blob(blob_id), raw)?;

        Ok(())
    }

    /// Get a list of IDs for all blobs which have not yet been retrieved.
    pub fn get_pending_blobs(&self) -> Result<Vec<String>> {
        let mut list = Vec::new();

        let db = self.db.as_ref().unwrap();
        let scan_key: &[u8] = &[PREFIX_BLOB];
        for item in db.range(scan_key..) {
            let (k, v) = item?;
            let blob: BlobStatus = serde_cbor::from_slice(&v)?;
            if !blob.retrieved {
                list.push(String::from_utf8_lossy(&k[1..]).to_string());
            }
        }

        Ok(list)
    }

    /// Get the sequence number of the latest message in the feed authored by
    /// the peer with the given public key.
    pub fn get_latest_seq(&self, user_id: &str) -> Result<Option<u64>> {
        let db = self.db.as_ref().unwrap();
        let key = Self::key_latest_seq(user_id);
        let seq = if let Some(value) = db.get(&key)? {
            let mut u64_buffer = [0u8; 8];
            u64_buffer.copy_from_slice(&value);
            Some(u64::from_be_bytes(u64_buffer))
        } else {
            None
        };

        Ok(seq)
    }

    /// Get the message KVT (Key Value Timestamp) for the given message ID
    /// (key).
    pub fn get_msg_kvt(&self, user_id: &str, msg_seq: u64) -> Result<Option<MessageKVT>> {
        let db = self.db.as_ref().unwrap();
        if let Some(raw) = db.get(Self::key_msg_kvt(user_id, msg_seq))? {
            Ok(Some(MessageKVT::from_slice(&raw)?))
        } else {
            Ok(None)
        }
    }

    /// Get the message value for the given message ID (key).
    pub fn get_msg_val(&self, msg_id: &str) -> Result<Option<MessageValue>> {
        let db = self.db.as_ref().unwrap();

        if let Some(raw) = db.get(Self::key_msg_val(msg_id))? {
            let msg_ref = serde_cbor::from_slice::<PubKeyAndSeqNum>(&raw)?;
            let msg = self
                .get_msg_kvt(&msg_ref.pub_key, msg_ref.seq_num)?
                .unwrap()
                .into_message()?;
            Ok(Some(msg))
        } else {
            Ok(None)
        }
    }

    /// Get the latest message value authored by the given public key.
    pub fn get_latest_msg_val(&self, user_id: &str) -> Result<Option<MessageValue>> {
        let latest_msg = if let Some(last_id) = self.get_latest_seq(user_id)? {
            Some(
                self.get_msg_kvt(user_id, last_id)?
                    .unwrap()
                    .into_message()?,
            )
        } else {
            None
        };

        Ok(latest_msg)
    }

    /// Add the public key and latest sequence number of a peer to the list of
    /// peers.
    pub async fn set_peer(&self, user_id: &str, latest_seq: u64) -> Result<()> {
        let db = self.db.as_ref().unwrap();
        db.insert(Self::key_peer(user_id), &latest_seq.to_be_bytes()[..])?;

        // TODO: Should we be flushing here?
        // Flush may have a performance impact. It may also be unnecessary
        // depending on where / when this method is called.

        Ok(())
    }

    /// Return the public key and latest sequence number for all peers in the
    /// database.
    pub async fn get_peers(&self) -> Result<Vec<PubKeyAndSeqNum>> {
        let db = self.db.as_ref().unwrap();
        let mut peers = Vec::new();

        // Use the generic peer prefix to return an iterator over all peers.
        let scan_peer_key: &[u8] = &[PREFIX_PEER];
        for peer in db.range(scan_peer_key..) {
            let (peer_key, _) = peer?;
            // Drop the prefix byte and convert the remaining bytes to
            // a string.
            let pub_key = String::from_utf8_lossy(&peer_key[1..]).to_string();
            // Get the latest sequence number for the peer.
            let seq_num = self.get_latest_seq(&pub_key)?.map_or(0, |num| num) + 1;
            let peer_latest_sequence = PubKeyAndSeqNum { pub_key, seq_num };
            peers.push(peer_latest_sequence)
        }

        Ok(peers)
    }

    /// Append a message value to a feed.
    pub async fn append_feed(&self, msg_val: MessageValue) -> Result<u64> {
        let seq_num = self.get_latest_seq(msg_val.author())?.map_or(0, |num| num) + 1;

        // TODO: We should really be performing more comprehensive validation.
        // Are there other checks in place behind the scenes?
        if msg_val.sequence() != seq_num {
            return Err(Error::InvalidSequence);
        }

        let author = msg_val.author().to_owned();
        let db = self.db.as_ref().unwrap();

        let msg_ref = serde_cbor::to_vec(&PubKeyAndSeqNum {
            pub_key: author.clone(),
            seq_num,
        })?;
        db.insert(Self::key_msg_val(&msg_val.id().to_string()), msg_ref)?;

        let msg_kvt = MessageKVT::new(msg_val.clone());
        db.insert(
            Self::key_msg_kvt(&author, seq_num),
            msg_kvt.to_string().as_bytes(),
        )?;
        db.insert(Self::key_latest_seq(&author), &seq_num.to_be_bytes()[..])?;

        // Add the public key and latest sequence number for this peer to the
        // list of peers.
        self.set_peer(&author, seq_num).await?;

        db.flush_async().await?;

        let broker_msg = BrokerEvent::new(
            Destination::Broadcast,
            StoKvEvent::IdChanged(msg_val.author().clone()),
        );

        self.ch_broker
            .as_ref()
            .unwrap()
            .send(broker_msg)
            .await
            .unwrap();

        Ok(seq_num)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use sled::Config as KvConfig;

    #[test]
    fn test_blobs() -> Result<()> {
        let mut kv = KvStorage::default();
        let (sender, _) = futures::channel::mpsc::unbounded();
        let path = tempdir::TempDir::new("solardb").unwrap();
        let config = KvConfig::new().path(&path.path());
        kv.open(config, sender).unwrap();

        assert_eq!(true, kv.get_blob("1").unwrap().is_none());

        kv.set_blob(
            "b1",
            &BlobStatus {
                retrieved: true,
                users: ["u1".to_string()].to_vec(),
            },
        )
        .unwrap();

        kv.set_blob(
            "b2",
            &BlobStatus {
                retrieved: false,
                users: ["u2".to_string()].to_vec(),
            },
        )
        .unwrap();

        let blob = kv.get_blob("b1").unwrap().unwrap();

        assert_eq!(blob.retrieved, true);
        assert_eq!(blob.users, ["u1".to_string()].to_vec());
        assert_eq!(kv.get_pending_blobs().unwrap(), ["b2".to_string()].to_vec());

        kv.set_blob(
            "b1",
            &BlobStatus {
                retrieved: false,
                users: ["u7".to_string()].to_vec(),
            },
        )
        .unwrap();

        let blob = kv.get_blob("b1").unwrap().unwrap();

        assert_eq!(blob.retrieved, false);
        assert_eq!(blob.users, ["u7".to_string()].to_vec());
        assert_eq!(
            kv.get_pending_blobs().unwrap(),
            ["b1".to_string(), "b2".to_string()].to_vec()
        );

        Ok(())
    }
}
