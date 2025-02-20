//! Epidemic Broadcast Tree (EBT) Replication Handler.

use std::marker::PhantomData;

use async_std::io::Write;
use futures::SinkExt;
use kuska_ssb::{
    api::{
        dto::{self},
        ApiCaller, ApiMethod,
    },
    feed::{Feed as MessageKvt, Message},
    rpc,
};
use log::{trace, warn};

use crate::{
    actors::{
        muxrpc::{ReqNo, RpcInput},
        replication::ebt::{EbtEvent, SessionRole},
    },
    broker::{BrokerEvent, BrokerMessage, ChBrokerSend, Destination, BROKER},
    error::Error,
    Result,
};

/// EBT replicate handler. Tracks active requests and peer connections.
pub struct EbtReplicateHandler<W>
where
    W: Write + Unpin + Send + Sync,
{
    /// EBT-related requests which are known and allowed.
    // TODO: Include connection ID as key. Then we can remove request ID from
    // all `EbtEvent` variants and simply look-up the request ID associated
    // with the connection ID (as defined in the `EbtEvent` data).
    active_request: ReqNo,
    phantom: PhantomData<W>,
}

impl<W> EbtReplicateHandler<W>
where
    W: Write + Unpin + Send + Sync,
{
    /// Instantiate a new instance of `EbtReplicateHandler`.
    pub fn new() -> Self {
        Self {
            active_request: 0,
            phantom: PhantomData,
        }
    }

    /// Handle an RPC event.
    pub async fn handle(
        &mut self,
        api: &mut ApiCaller<W>,
        op: &RpcInput,
        ch_broker: &mut ChBrokerSend,
        peer_ssb_id: String,
        connection_id: usize,
        active_req_no: Option<ReqNo>,
    ) -> Result<bool> {
        trace!(target: "muxrpc-ebt-handler", "Received MUXRPC input: {:?}", op);

        // An outbound EBT replicate request was made before the handler was
        // called.
        if let Some(req_no) = active_req_no {
            self.active_request = req_no
        }

        match op {
            // Handle an incoming MUXRPC request.
            RpcInput::Network(req_no, rpc::RecvMsg::RpcRequest(req)) => {
                self.recv_rpc_request(api, *req_no, req, peer_ssb_id, connection_id)
                    .await
            }
            // Hanlde an incoming 'other' MUXRPC request.
            RpcInput::Network(req_no, rpc::RecvMsg::OtherRequest(_type, req)) => {
                self.recv_other_request(ch_broker, *req_no, req, peer_ssb_id, connection_id)
                    .await
            }
            // Handle an incoming MUXRPC response.
            RpcInput::Network(req_no, rpc::RecvMsg::RpcResponse(_type, res)) => {
                self.recv_rpc_response(ch_broker, *req_no, res, peer_ssb_id, connection_id)
                    .await
            }
            // Handle an incoming MUXRPC 'cancel stream' response.
            RpcInput::Network(req_no, rpc::RecvMsg::CancelStreamResponse()) => {
                self.recv_cancelstream(api, *req_no).await
            }
            // Handle an incoming MUXRPC error response.
            RpcInput::Network(req_no, rpc::RecvMsg::ErrorResponse(err)) => {
                self.recv_error_response(*req_no, err).await
            }
            // Handle a broker message.
            RpcInput::Message(msg) => match msg {
                BrokerMessage::Ebt(EbtEvent::TerminateSession(conn_id, session_role)) => {
                    if conn_id == &connection_id {
                        let req_no = match session_role {
                            SessionRole::Requester => self.active_request,
                            SessionRole::Responder => -(self.active_request),
                        };

                        return self.send_cancelstream(api, req_no).await;
                    }

                    Ok(false)
                }
                BrokerMessage::Ebt(EbtEvent::SendClock(conn_id, req_no, clock, session_role)) => {
                    // This is, regrettably, rather unintuitive.
                    //
                    // `api.ebt_clock_res_send()` internally calls
                    // `send_response()` which applies the negative sign to the
                    // given request number. However, the request number should
                    // always be positive when we are acting as the requester -
                    // even though we are sending a "response". This is why we
                    // apply a negative here for the session requester: so that
                    // the double negation results in a response with a
                    // positive request number.
                    let req_no = match session_role {
                        SessionRole::Requester => -(*req_no),
                        SessionRole::Responder => *req_no,
                    };

                    // Only send the clock if the associated connection is
                    // being handled by this instance of the handler.
                    //
                    // This prevents the clock being sent to every peer with
                    // whom we have an active session and matching request
                    // number.
                    if *conn_id == connection_id {
                        // Serialize the vector clock as a JSON string.
                        let json_clock = serde_json::to_string(&clock)?;
                        // The request number must be negative (response).
                        api.ebt_clock_res_send(req_no, &json_clock).await?;

                        trace!(target: "ebt", "Sent clock to connection {} with request number {} as {}", conn_id, req_no, session_role);
                    }

                    Ok(false)
                }
                BrokerMessage::Ebt(EbtEvent::SendMessage(
                    conn_id,
                    req_no,
                    ssb_id,
                    msg,
                    session_role,
                )) => {
                    // Define the sign of the request number based on session
                    // role (note: this is the opposite sign of the number
                    // that will ultimately be sent.
                    //
                    // See the comment in the `SendClock` event above for
                    // further explantation.
                    let req_no = match session_role {
                        SessionRole::Requester => -(*req_no),
                        SessionRole::Responder => *req_no,
                    };

                    // Only send the message if the associated connection is
                    // being handled by this instance of the handler.
                    //
                    // This prevents the message being sent to every peer with
                    // whom we have an active session and matching request
                    // number.
                    if *conn_id == connection_id {
                        let json_msg = msg.to_string();
                        api.ebt_feed_res_send(req_no, &json_msg).await?;

                        trace!(target: "ebt", "Sent message to {} on connection {}", ssb_id, conn_id);
                    }

                    Ok(false)
                }
                _ => Ok(false),
            },
            _ => Ok(false),
        }
    }

    /// Process an incoming MUXRPC request.
    async fn recv_rpc_request(
        &mut self,
        api: &mut ApiCaller<W>,
        req_no: ReqNo,
        req: &rpc::Body,
        peer_ssb_id: String,
        connection_id: usize,
    ) -> Result<bool> {
        match ApiMethod::from_rpc_body(req) {
            Some(ApiMethod::EbtReplicate) => {
                self.recv_ebtreplicate(api, req_no, req, peer_ssb_id, connection_id)
                    .await
            }
            _ => Ok(false),
        }
    }

    /// Process and respond to an incoming EBT replicate request.
    async fn recv_ebtreplicate(
        &mut self,
        api: &mut ApiCaller<W>,
        req_no: ReqNo,
        req: &rpc::Body,
        peer_ssb_id: String,
        connection_id: usize,
    ) -> Result<bool> {
        // Deserialize the args from an incoming EBT replicate request.
        let mut args: Vec<dto::EbtReplicate> = serde_json::from_value(req.args.clone())?;
        trace!(target: "ebt-handler", "Received replicate request: {:?}", args);

        // Retrieve the `EbtReplicate` args from the array.
        let args = args.pop().unwrap();

        let mut ch_broker = BROKER.lock().await.create_sender();

        // Validate the EBT request args (`version` and `format`).
        // Terminate the stream with an error response if expectations are
        // not met.
        if !args.version == 3 {
            let err_msg = String::from("ebt version != 3");
            api.rpc().send_error(req_no, req.rpc_type, &err_msg).await?;

            return Err(Error::EbtReplicate((req_no, err_msg)));
        } else if args.format.as_str() != "classic" {
            let err_msg = String::from("ebt format != classic");
            api.rpc().send_error(req_no, req.rpc_type, &err_msg).await?;

            return Err(Error::EbtReplicate((req_no, err_msg)));
        }

        trace!(target: "ebt-handler", "Successfully validated replicate request arguments");

        // Set the request number for this session.
        self.active_request = req_no;

        ch_broker
            .send(BrokerEvent::new(
                Destination::Broadcast,
                BrokerMessage::Ebt(EbtEvent::SessionInitiated(
                    connection_id,
                    req_no,
                    peer_ssb_id,
                    SessionRole::Responder,
                )),
            ))
            .await?;

        Ok(false)
    }

    /// Process an incoming MUXRPC request containing a vector clock.
    async fn recv_other_request(
        &mut self,
        ch_broker: &mut ChBrokerSend,
        req_no: ReqNo,
        req: &[u8],
        peer_ssb_id: String,
        connection_id: usize,
    ) -> Result<bool> {
        // Attempt to deserialize bytes into vector clock hashmap.
        // If the deserialization is successful, emit a 'received clock'
        // event.
        if let Ok(clock) = serde_json::from_slice(req) {
            ch_broker
                .send(BrokerEvent::new(
                    Destination::Broadcast,
                    BrokerMessage::Ebt(EbtEvent::ReceivedClock(
                        connection_id,
                        req_no,
                        peer_ssb_id,
                        clock,
                    )),
                ))
                .await?;
        }

        Ok(false)
    }

    /// Process an incoming MUXRPC response.
    /// The response is expected to contain a vector clock or an SSB message.
    async fn recv_rpc_response(
        &mut self,
        ch_broker: &mut ChBrokerSend,
        req_no: ReqNo,
        res: &[u8],
        peer_ssb_id: String,
        connection_id: usize,
    ) -> Result<bool> {
        trace!(target: "ebt-handler", "Received RPC response: {}", req_no);

        // Only handle the response if the associated request number is known
        // to us, either because we sent or received the initiating replicate
        // request.
        if self.active_request == req_no || self.active_request == -(req_no) {
            // The response may be a vector clock (aka. notes) or an SSB message.
            //
            // Since there is no explicit way to determine which was received,
            // we first attempt deserialization of a vector clock and move on
            // to attempting message deserialization if that fails.
            if let Ok(clock) = serde_json::from_slice(res) {
                ch_broker
                    .send(BrokerEvent::new(
                        Destination::Broadcast,
                        BrokerMessage::Ebt(EbtEvent::ReceivedClock(
                            connection_id,
                            req_no,
                            peer_ssb_id,
                            clock,
                        )),
                    ))
                    .await?;
            } else {
                // First try to deserialize the response into a message value.
                // If that fails, try to deserialize into a message KVT and then
                // convert that into a message value. Return an error if that fails.
                // This approach allows us to handle the unlikely event that
                // messages are sent as KVTs and not simply values.
                //
                // Validation of the message signature and fields is also performed
                // as part of the call to `from_slice`.
                let msg = match Message::from_slice(res) {
                    Ok(msg) => msg,
                    Err(_) => MessageKvt::from_slice(res)?.into_message()?,
                };

                ch_broker
                    .send(BrokerEvent::new(
                        Destination::Broadcast,
                        BrokerMessage::Ebt(EbtEvent::ReceivedMessage(msg)),
                    ))
                    .await?;
            }
        }

        Ok(false)
    }

    /// Receive close-stream request.
    async fn recv_cancelstream(&mut self, api: &mut ApiCaller<W>, req_no: ReqNo) -> Result<bool> {
        trace!(target: "ebt-handler", "Received cancel stream RPC response: {}", req_no);

        api.rpc().send_stream_eof(-req_no).await?;

        Ok(true)
    }

    /// Send close-stream request.
    async fn send_cancelstream(&mut self, api: &mut ApiCaller<W>, req_no: ReqNo) -> Result<bool> {
        trace!(target: "ebt-handler", "Send cancel stream RPC response: {}", req_no);

        api.rpc().send_stream_eof(-req_no).await?;

        Ok(true)
    }

    /// Report a MUXRPC error and remove the associated request from the map of
    /// active requests.
    async fn recv_error_response(&mut self, req_no: ReqNo, err_msg: &str) -> Result<bool> {
        warn!("Received MUXRPC error response: {}", err_msg);

        Err(Error::EbtReplicate((req_no, err_msg.to_string())))
    }
}
