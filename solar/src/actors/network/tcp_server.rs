use async_std::{
    net::{TcpListener, ToSocketAddrs},
    prelude::*,
};
use futures::{select_biased, FutureExt};
use kuska_ssb::keystore::OwnedIdentity;
use log::debug;

use crate::{
    actors::network::{connection, connection::TcpConnection},
    broker::*,
    Result,
};

pub async fn actor(
    server_id: OwnedIdentity,
    addr: impl ToSocketAddrs,
    selective_replication: bool,
) -> Result<()> {
    let broker = BROKER.lock().await.register("tcp-server", false).await?;

    let mut ch_terminate = broker.ch_terminate.fuse();

    let listener = TcpListener::bind(addr).await?;
    let mut incoming = listener.incoming();
    debug!("Listening for inbound TCP connection...");

    loop {
        select_biased! {
            _ = ch_terminate => break,
            stream = incoming.next().fuse() => {
                if let Some(stream) = stream {
                    if let Ok(stream) = stream {
                        debug!("Received inbound TCP connection");
                        Broker::spawn(
                            connection::actor(
                                TcpConnection::Listen { stream },
                                server_id.clone(),
                                selective_replication
                            )
                        );
                    }
                } else {
                    break;
                }
            },
        }
    }

    let _ = broker.ch_terminated.send(Void {});

    Ok(())
}
