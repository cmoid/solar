use std::net::SocketAddr;

use async_std::sync::{Arc, RwLock};
use futures::SinkExt;
use kuska_ssb::crypto::{ed25519::PublicKey, ToSodiumObject};
use once_cell::sync::Lazy;

use crate::{
    actors::{
        jsonrpc,
        network::{
            connection_manager::CONNECTION_MANAGER, connection_scheduler, lan_discovery, tcp_server,
        },
    },
    broker::*,
    config::ApplicationConfig,
    storage::{blob::BlobStorage, kv::KvStorage},
    Result,
};

// Instantiate the key-value store.
pub static KV_STORE: Lazy<Arc<RwLock<KvStorage>>> =
    Lazy::new(|| Arc::new(RwLock::new(KvStorage::default())));
// Instantiate the blob store.
pub static BLOB_STORE: Lazy<Arc<RwLock<BlobStorage>>> =
    Lazy::new(|| Arc::new(RwLock::new(BlobStorage::default())));

/// Main runtime managing the solar node process.
pub struct Node;

impl Node {
    /// Start the solar node with full storage and networking capabilities.
    pub async fn start(config: ApplicationConfig) -> Result<()> {
        // Open the key-value store using the given configuration parameters and
        // an unbounded sender channel for message passing.
        KV_STORE
            .write()
            .await
            .open(config.database, BROKER.lock().await.create_sender())?;

        // Define the directory name for the blob store.
        let blobs_path = config
            .base_path
            .expect("Base path not supplied")
            .join("blobs");

        // Open the blobstore using the given folder path and an unbounded sender
        // channel for message passing.
        BLOB_STORE
            .write()
            .await
            .open(blobs_path, BROKER.lock().await.create_sender());

        // Spawn the ctrlc actor. Listens for SIGINT termination signal.
        Broker::spawn(crate::actors::ctrlc::actor());

        // Print 'starting server' announcement.
        println!(
            "Starting TCP server on {}:{}:{}",
            &config.network.ip,
            &config.network.port,
            base64::encode(&config.secret.private_key[..]),
        );

        // Construct the TCP server listening address.
        let tcp_server_addr: SocketAddr =
            format!("{}:{}", config.network.ip, config.network.port).parse()?;

        // Spawn the TCP server. Facilitates peer connections.
        Broker::spawn(tcp_server::actor(
            config.secret.to_owned_identity()?,
            tcp_server_addr,
            config.replication.selective,
        ));

        // Print the network key.
        println!(
            "Node deploy on network: {}",
            hex::encode(config.network.key)
        );

        // Construct the JSON-RPC server listening address.
        let jsonrpc_server_addr: SocketAddr =
            format!("{}:{}", config.jsonrpc.ip, config.jsonrpc.port).parse()?;

        // Spawn the JSON-RPC server if the option has been set to true in the
        // CLI arguments. Facilitates operator queries during runtime.
        if config.jsonrpc.server {
            Broker::spawn(jsonrpc::server::actor(
                config.secret.to_owned_identity()?,
                jsonrpc_server_addr,
            ));
        }

        // Spawn the LAN discovery actor. Listens for and broadcasts UDP packets
        // to allow LAN-local peer connections.
        if config.network.lan_discovery {
            Broker::spawn(lan_discovery::actor(
                config.secret.to_owned_identity()?,
                config.network.port,
                config.replication.selective,
            ));
        }

        // Convert the HashMap of peers to be replicated into a Vec.
        let mut peers_to_dial: Vec<(PublicKey, String)> = config
            .replication
            .peers
            .into_iter()
            // TODO: validate all public keys in `ReplicationConfig`
            .map(|(public_key, url)| (public_key.to_ed25519_pk().unwrap(), url))
            .collect();

        // Add any connection details supplied via the `--connect` CLI option.
        peers_to_dial.extend(config.network.connect);

        println!("Peers to dial: {:?}", peers_to_dial);

        // Spawn the connection scheduler actor. Dials remote peers on an
        // ongoing basis (at `eager` or `lazy` intervals).
        Broker::spawn(connection_scheduler::actor(
            peers_to_dial,
            config.replication.selective,
        ));

        // Spawn the connection manager message loop.
        let connection_manager_msgloop = CONNECTION_MANAGER.write().await.take_msgloop();
        connection_manager_msgloop.await;

        // Spawn the broker message loop.
        let broker_msgloop = BROKER.lock().await.take_msgloop();
        broker_msgloop.await;

        println!("Gracefully finished");

        Ok(())
    }

    /// Shutdown the node by sending a termination signal to all actors.
    pub async fn shutdown() {
        // Create a sender channel to pass messages to the broker message loop.
        let mut sender = BROKER.lock().await.create_sender();

        // Send the termination event signal.
        let _ = sender.send(BrokerEvent::Terminate).await;
    }
}
