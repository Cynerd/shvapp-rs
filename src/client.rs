//! Minimal Redis client implementation
//!
//! Provides an async connect and methods for issuing the supported commands.

// use crate::cmd::{Get, Publish, Set, Subscribe, Unsubscribe};
use crate::{Connection};

use tokio::net::{TcpStream};
use tracing::{debug, info, warn, error};
use chainpack::{RpcMessage, RpcMessageMetaTags, RpcValue};
use crate::frame::Protocol;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const DEFAULT_RPC_CALL_TIMEOUT_MS: u64 = 5000;

#[derive(Copy, Clone)]
pub enum PasswordType {
    PLAIN,
    SHA1
}
impl PasswordType {
    pub fn to_str(&self) -> &str {
        match self {
            PasswordType::PLAIN => "PLAIN",
            PasswordType::SHA1 => "SHA1",
        }
    }
 }

#[derive(Clone)]
pub struct ConnectionParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub password_type: PasswordType,
    pub idle_watchdog_timeout: i32,
    pub protocol: Protocol,
}
impl ConnectionParams {
    pub fn new(host: &str, port: u16, user: &str, password: &str) -> ConnectionParams {
        ConnectionParams {
            host: host.into(),
            port,
            user: user.into(),
            password: password.into(),
            password_type: if user.len() == 40 { PasswordType::SHA1} else { PasswordType::PLAIN },
            idle_watchdog_timeout: 180,
            protocol: Protocol::ChainPack,
        }
    }
    fn to_rpcvalue(&self) -> RpcValue {
        let mut map = chainpack::rpcvalue::Map::new();
        let mut login = chainpack::rpcvalue::Map::new();
        login.insert("user".into(), RpcValue::new(&self.user));
        login.insert("password".into(), RpcValue::new(&self.password));
        login.insert("type".into(), RpcValue::new(self.password_type.to_str()));
        map.insert("login".into(), RpcValue::new(login));
        let mut options = chainpack::rpcvalue::Map::new();
        options.insert("idleWatchDogTimeOut".into(), RpcValue::new(self.idle_watchdog_timeout));
        map.insert("options".into(), RpcValue::new(options));
        RpcValue::new(map)
    }
}

pub struct ClientConnection {
    connection: Connection,
    send_request_rx: mpsc::Receiver<RpcMessage>,
    recv_responsetx: watch::Sender<RpcMessage>,
}

pub struct Client {
    pub send_request_tx: mpsc::Sender<RpcMessage>,
    pub recv_response_rx: watch::Receiver<RpcMessage>,
}

pub struct Request {
    pub send_request_tx: mpsc::Sender<RpcMessage>,
    pub recv_response_rx: watch::Receiver<RpcMessage>,
}

pub struct MessageNotifier {
    pub recv_response_rx: watch::Receiver<RpcMessage>,
}

pub async fn connect(params: &ConnectionParams) -> crate::Result<(Client, ClientConnection)> {
    // The `addr` argument is passed directly to `TcpStream::connect`. This
    // performs any asynchronous DNS lookup and attempts to establish the TCP
    // connection. An error at either step returns an error, which is then
    // bubbled up to the caller of `mini_redis` connect.
    let addr = format!("{}:{}", params.host, params.port);
    info!("connecting to: {}", addr);
    let socket = TcpStream::connect(addr.clone()).await?;

    info!("connected to: {}", addr);
    // Initialize the connection state. This allocates read/write buffers to
    // perform redis protocol frame parsing.
    let mut connection = Connection::new(socket);
    connection.protocol = Some(params.protocol);

    const BUFF_LEN: usize = 1024;
    let (send_request_tx, send_request_rx) = mpsc::channel(BUFF_LEN);
    let (recv_responsetx, recv_response_rx) = watch::channel(RpcMessage::default());
    Ok((
        Client {
            send_request_tx,
            recv_response_rx,
        },
        ClientConnection {
            connection,
            send_request_rx,
            recv_responsetx,
        },
    ))
}

impl ClientConnection {
    pub async fn exec(&mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                resp = self.connection.recv_message() => {
                    match resp {
                        Ok(resp) => {
                            // debug!(?maybe_resp);
                            info!("message received: {}", resp);
                            self.recv_responsetx.send(resp)?;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                rq = self.send_request_rx.recv() => {
                    match rq {
                        Some(rq) => {
                            info!("send request: {}", rq);
                            self.connection.send_message(&rq).await?;
                        }
                        None => {
                            info!("Ignoring empty request, client disconnected?");

                        }
                    }
                }
            }
        }
    }
}

impl Client {

    pub async fn login(&mut self, login_params: &ConnectionParams) -> crate::Result<()> {
        let mut rq = self.create_request();
        let hello_resp = rq.exec(RpcMessage::new_request("", "hello", None)).await?;
        debug!("hello resp {}", hello_resp);
        let mut login_params = login_params.clone();
        if login_params.password.len() != 40 {
            if let Some(result) = hello_resp.result() {
                if let Some(nonce) = result.as_map().get("nonce") {
                    let hash = crate::utils::sha1_password_hash(&login_params.password, nonce.as_str());
                    login_params.password = hash;
                    login_params.password_type = PasswordType::SHA1;
                } else {
                    warn!("nonce param missing!");
                }
            } else {
                warn!("hello response params missing!");
            }
        }
        let mut rq = self.create_request();
        let login_resp = rq.exec(RpcMessage::new_request("", "login", Some(login_params.to_rpcvalue()))).await?;
        debug!("login result: {}", login_resp);
        match login_resp.result() {
            Some(_) => {
                let heartbeat_interval = login_params.idle_watchdog_timeout as u64 / 3;
                if heartbeat_interval >= 60 {
                    let mut ping_rq = self.create_request();
                    tokio::spawn(async move {
                        info!("Starting heart-beat task with period: {}", heartbeat_interval);
                        loop {
                            tokio::time::sleep(Duration::from_secs(heartbeat_interval)).await;
                            debug!("Sending heart beat");
                            let res = ping_rq.exec(RpcMessage::new_request(".broker/app", "ping", None)).await;
                            match res {
                                Ok(_) => {}
                                Err(e) => error!("cannot send ping: {}", e),
                            }
                        }
                    });
                }
                Ok(())
            },
            None => Err("Login incorrect!".into())
        }
    }

    pub fn create_request(&self) -> Request {
        Request {
            send_request_tx: self.send_request_tx.clone(),
            recv_response_rx: self.recv_response_rx.clone(),
        }
    }
    pub fn create_message_notifier(&self) -> MessageNotifier {
        MessageNotifier {
            recv_response_rx: self.recv_response_rx.clone(),
        }
    }
}

impl Request {
    pub async fn exec(&mut self, request: RpcMessage) -> crate::Result<RpcMessage> {
        if !request.is_request() {
            return Err("Not request".into())
        }
        let rq_id = request.request_id().ok_or("Request ID missing")?;
        debug!("sending RPC request id: {} .............. {}", rq_id, request);
        self.send_request_tx.send(request).await?;
        match tokio::time::timeout(Duration::from_millis(DEFAULT_RPC_CALL_TIMEOUT_MS), async move {
            loop {
                self.recv_response_rx.changed().await?;
                let resp = self.recv_response_rx.borrow();
                if let Some(id) = resp.request_id() {
                    if id == rq_id {
                        let resp = resp.clone();
                        debug!("{} .............. got response: {}", rq_id, resp);
                        return Ok(resp)
                    }
                }
            }
        }).await {
            Ok(resp) => resp,
            Err(_) => Err(format!("Response to request id: {} didn't arrive within {} msec.", rq_id, DEFAULT_RPC_CALL_TIMEOUT_MS).into()),
        }
    }
}

impl MessageNotifier {
    pub async fn wait_for_message(&mut self) -> crate::Result<RpcMessage> {
        self.recv_response_rx.changed().await?;
        let resp = self.recv_response_rx.borrow();
        let resp = resp.clone();
        return Ok(resp)
    }
    pub async fn wait_for_message_timeout(&mut self, timeout: Duration) -> crate::Result<RpcMessage> {
        tokio::time::timeout(timeout, self.wait_for_message()).await?
    }
}