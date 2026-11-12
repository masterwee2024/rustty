//! The SSH side of the app.
//!
//! Each terminal tab owns an `SshConnection`: a command sender consumed by a
//! tokio task, and an event receiver polled by the UI thread. No SSH state is
//! ever touched from the UI thread directly.

use crate::sessions::{host_key, AuthMethod, KnownHosts, Tunnel, TunnelKind};
use russh::keys::{load_secret_key, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{client, Channel, ChannelMsg, Disconnect};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub key_path: String,
    pub password: String,
    pub passphrase: String,
    pub terminal_type: String,
    pub keepalive_secs: u16,
    pub cols: u16,
    pub rows: u16,
    pub tunnels: Vec<Tunnel>,
}

impl ConnectOptions {
    pub fn target(&self) -> String {
        if self.port == 22 {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[derive(Clone, Debug)]
pub struct HostKeyInfo {
    pub host_key: String,
    pub display_host: String,
    pub algorithm: String,
    pub fingerprint: String,
    pub previous: Option<String>,
}

#[derive(Debug)]
pub enum SshEvent {
    Banner(String),
    HostKey(HostKeyInfo),
    Connected,
    Data(Vec<u8>),
    TunnelStatus {
        index: usize,
        message: String,
        active: bool,
    },
    Closed(String),
    Error(String),
}

#[derive(Debug)]
pub enum SshCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
    Disconnect,
}

/// Sends an event to the UI thread and wakes it up so it repaints promptly.
#[derive(Clone)]
struct EventSink {
    tx: Sender<SshEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn send(&self, event: SshEvent) -> bool {
        let sent = self.tx.send(event).is_ok();
        (self.wake)();
        sent
    }
}

pub struct SshConnection {
    pub tx: UnboundedSender<SshCommand>,
    pub rx: Receiver<SshEvent>,
}

impl SshConnection {
    pub fn send(&self, command: SshCommand) -> bool {
        self.tx.send(command).is_ok()
    }

    pub fn disconnect(&self) {
        let _ = self.tx.send(SshCommand::Disconnect);
    }
}

pub fn start(
    runtime: &tokio::runtime::Runtime,
    options: ConnectOptions,
    known_hosts: KnownHosts,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> SshConnection {
    let (cmd_tx, cmd_rx) = unbounded_channel();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel();
    let events = EventSink { tx: evt_tx, wake };
    runtime.spawn(run(options, known_hosts, cmd_rx, events));
    SshConnection {
        tx: cmd_tx,
        rx: evt_rx,
    }
}

struct ClientHandler {
    host_key: String,
    display_host: String,
    known_hosts: KnownHosts,
    events: EventSink,
    rejected_host_key: Arc<AtomicBool>,
    remote_tunnels: Vec<RemoteTunnel>,
}

#[derive(Clone, Debug)]
struct RemoteTunnel {
    bind_host: String,
    bind_port: u16,
    target_host: String,
    target_port: u16,
}

impl RemoteTunnel {
    fn matches(&self, address: &str, port: u32) -> bool {
        if u32::from(self.bind_port) != port {
            return false;
        }
        let address = address.trim().to_ascii_lowercase();
        let configured = self.bind_host.trim().to_ascii_lowercase();
        if address == configured {
            return true;
        }
        if configured.is_empty()
            || configured == "0.0.0.0"
            || configured == "::"
            || configured == "*"
        {
            return true;
        }
        let loopback = |host: &str| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]");
        loopback(&address) && loopback(&configured)
    }
}

struct ForwardRequest {
    target_host: String,
    target_port: u16,
    origin_address: String,
    origin_port: u32,
    reply: oneshot::Sender<Result<Channel<client::Msg>, String>>,
}

/// Aborts forwarding listeners when the SSH connection ends.
struct ListenerGuard(Vec<JoinHandle<()>>);

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let public_key = server_public_key.public_key();
        let algorithm = public_key.algorithm().to_string();
        let fingerprint = public_key
            .fingerprint(russh::keys::HashAlg::Sha256)
            .to_string();

        match self.known_hosts.get(&self.host_key) {
            Some(known) if known.fingerprint == fingerprint => Ok(true),
            other => {
                let _ = self.events.send(SshEvent::HostKey(HostKeyInfo {
                    host_key: self.host_key.clone(),
                    display_host: self.display_host.clone(),
                    algorithm,
                    fingerprint,
                    previous: other.map(|known| known.fingerprint.clone()),
                }));
                self.rejected_host_key.store(true, Ordering::SeqCst);
                Ok(false)
            }
        }
    }

    async fn auth_banner(
        &mut self,
        banner: &str,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let _ = self.events.send(SshEvent::Banner(banner.to_owned()));
        Ok(())
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: Channel<client::Msg>,
        connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let target = self
            .remote_tunnels
            .iter()
            .find(|tunnel| tunnel.matches(connected_address, connected_port))
            .cloned();

        if let Some(target) = target {
            reply.accept().await;
            tokio::spawn(async move {
                match TcpStream::connect((target.target_host.as_str(), target.target_port)).await {
                    Ok(stream) => bridge(channel, stream).await,
                    Err(_) => {
                        let _ = channel.close().await;
                    }
                }
            });
        }
        Ok(())
    }
}

async fn run(
    options: ConnectOptions,
    known_hosts: KnownHosts,
    mut commands: UnboundedReceiver<SshCommand>,
    events: EventSink,
) {
    let rejected_host_key = Arc::new(AtomicBool::new(false));
    let result = connect_and_run(
        &options,
        known_hosts,
        &mut commands,
        &events,
        rejected_host_key.clone(),
    )
    .await;

    if let Err(error) = result {
        if !rejected_host_key.load(Ordering::SeqCst) {
            let _ = events.send(SshEvent::Error(error));
        }
    }
}

async fn connect_and_run(
    options: &ConnectOptions,
    known_hosts: KnownHosts,
    commands: &mut UnboundedReceiver<SshCommand>,
    events: &EventSink,
    rejected_host_key: Arc<AtomicBool>,
) -> Result<(), String> {
    let config = client::Config {
        inactivity_timeout: None,
        keepalive_interval: (options.keepalive_secs > 0)
            .then(|| Duration::from_secs(u64::from(options.keepalive_secs))),
        nodelay: true,
        ..Default::default()
    };

    let handler = ClientHandler {
        host_key: host_key(&options.host, options.port),
        display_host: options.target(),
        known_hosts,
        events: events.clone(),
        rejected_host_key,
        remote_tunnels: options
            .tunnels
            .iter()
            .filter(|tunnel| tunnel.kind == TunnelKind::Remote)
            .map(|tunnel| RemoteTunnel {
                bind_host: tunnel.bind_host.clone(),
                bind_port: tunnel.bind_port,
                target_host: tunnel.target_host.clone(),
                target_port: tunnel.target_port,
            })
            .collect(),
    };

    let mut session = tokio::time::timeout(
        Duration::from_secs(20),
        client::connect(
            Arc::new(config),
            (options.host.as_str(), options.port),
            handler,
        ),
    )
    .await
    .map_err(|_| "Connection timed out after 20 seconds.".to_owned())?
    .map_err(|error| format!("Could not connect: {error}"))?;

    let authenticated =
        tokio::time::timeout(Duration::from_secs(45), authenticate(&mut session, options))
            .await
            .map_err(|_| "Authentication timed out.".to_owned())??;

    if !authenticated {
        let _ = session
            .disconnect(Disconnect::ByApplication, "Authentication failed", "en")
            .await;
        return Err(
            "Authentication failed. Check your username, password and key settings.".to_owned(),
        );
    }

    let (forward_tx, mut forward_rx) = unbounded_channel::<ForwardRequest>();
    let _forward_keepalive = forward_tx.clone();
    let mut listener_tasks = Vec::new();

    for (index, tunnel) in options.tunnels.iter().enumerate() {
        match tunnel.kind {
            TunnelKind::Local => {
                match TcpListener::bind((tunnel.bind_host.as_str(), tunnel.bind_port)).await {
                    Ok(listener) => {
                        let address = listener
                            .local_addr()
                            .map(|address| address.to_string())
                            .unwrap_or_else(|_| {
                                format!("{}:{}", tunnel.bind_host, tunnel.bind_port)
                            });
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!("Listening on {address}"),
                            active: true,
                        });
                        listener_tasks.push(tokio::spawn(run_local_listener(
                            listener,
                            tunnel.target_host.clone(),
                            tunnel.target_port,
                            forward_tx.clone(),
                            events.clone(),
                            index,
                        )));
                    }
                    Err(error) => {
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!(
                                "Could not bind {}:{}: {error}",
                                tunnel.bind_host, tunnel.bind_port
                            ),
                            active: false,
                        });
                    }
                }
            }
            TunnelKind::Dynamic => {
                match TcpListener::bind((tunnel.bind_host.as_str(), tunnel.bind_port)).await {
                    Ok(listener) => {
                        let address = listener
                            .local_addr()
                            .map(|address| address.to_string())
                            .unwrap_or_else(|_| {
                                format!("{}:{}", tunnel.bind_host, tunnel.bind_port)
                            });
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!("SOCKS5 proxy listening on {address}"),
                            active: true,
                        });
                        listener_tasks.push(tokio::spawn(run_socks_listener(
                            listener,
                            forward_tx.clone(),
                            events.clone(),
                            index,
                        )));
                    }
                    Err(error) => {
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!(
                                "Could not bind {}:{}: {error}",
                                tunnel.bind_host, tunnel.bind_port
                            ),
                            active: false,
                        });
                    }
                }
            }
            TunnelKind::Remote => {
                match session
                    .tcpip_forward(
                        tunnel.bind_host.trim().to_owned(),
                        u32::from(tunnel.bind_port),
                    )
                    .await
                {
                    Ok(port) => {
                        let shown_port = if port == 0 {
                            u32::from(tunnel.bind_port)
                        } else {
                            port
                        };
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!(
                                "Remote listener active on {}:{shown_port}",
                                tunnel.bind_host
                            ),
                            active: true,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(SshEvent::TunnelStatus {
                            index,
                            message: format!("Remote forwarding was refused: {error}"),
                            active: false,
                        });
                    }
                }
            }
        }
    }

    let _listener_guard = ListenerGuard(listener_tasks);

    let mut channel = session
        .channel_open_session()
        .await
        .map_err(|error| format!("Could not open SSH channel: {error}"))?;

    channel
        .request_pty(
            false,
            &options.terminal_type,
            u32::from(options.cols.max(20)),
            u32::from(options.rows.max(5)),
            0,
            0,
            &[],
        )
        .await
        .map_err(|error| format!("Could not allocate a remote terminal: {error}"))?;

    channel
        .request_shell(true)
        .await
        .map_err(|error| format!("Could not start the remote shell: {error}"))?;

    let _ = events.send(SshEvent::Connected);

    loop {
        tokio::select! {
            request = forward_rx.recv() => {
                if let Some(ForwardRequest {
                    target_host,
                    target_port,
                    origin_address,
                    origin_port,
                    reply,
                }) = request
                {
                    let result = session
                        .channel_open_direct_tcpip(
                            target_host,
                            u32::from(target_port),
                            origin_address,
                            origin_port,
                        )
                        .await
                        .map_err(|error| error.to_string());
                    let _ = reply.send(result);
                }
            }
            command = commands.recv() => {
                match command {
                    Some(SshCommand::Input(data)) => {
                        if let Err(error) = channel.data(&data[..]).await {
                            return Err(format!("Connection lost while sending input: {error}"));
                        }
                    }
                    Some(SshCommand::Resize(cols, rows)) => {
                        let _ = channel
                            .window_change(u32::from(cols), u32::from(rows), 0, 0)
                            .await;
                    }
                    Some(SshCommand::Disconnect) | None => {
                        let _ = channel.eof().await;
                        break;
                    }
                }
            }
            message = channel.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        if !events.send(SshEvent::Data(data.to_vec())) {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if !events.send(SshEvent::Data(data.to_vec())) {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        let _ = events.send(SshEvent::Closed(format!("Remote shell exited ({exit_status})")));
                        break;
                    }
                    Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                        let _ = events.send(SshEvent::Closed(format!("Remote shell ended ({signal_name:?})")));
                        break;
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        let _ = events.send(SshEvent::Closed("Connection closed by server".to_owned()));
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = session
        .disconnect(Disconnect::ByApplication, "", "en")
        .await;
    Ok(())
}

async fn run_local_listener(
    listener: TcpListener,
    target_host: String,
    target_port: u16,
    forward_tx: UnboundedSender<ForwardRequest>,
    events: EventSink,
    index: usize,
) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let forward_tx = forward_tx.clone();
                let target_host = target_host.clone();
                tokio::spawn(async move {
                    let (reply, answer) = oneshot::channel();
                    let request = ForwardRequest {
                        target_host,
                        target_port,
                        origin_address: peer.ip().to_string(),
                        origin_port: u32::from(peer.port()),
                        reply,
                    };
                    if forward_tx.send(request).is_err() {
                        return;
                    }
                    if let Ok(Ok(channel)) = answer.await {
                        bridge(channel, stream).await;
                    }
                });
            }
            Err(error) => {
                let _ = events.send(SshEvent::TunnelStatus {
                    index,
                    message: format!("Forwarding listener stopped: {error}"),
                    active: false,
                });
                break;
            }
        }
    }
}

async fn run_socks_listener(
    listener: TcpListener,
    forward_tx: UnboundedSender<ForwardRequest>,
    events: EventSink,
    index: usize,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                tokio::spawn(handle_socks_connection(stream, forward_tx.clone()));
            }
            Err(error) => {
                let _ = events.send(SshEvent::TunnelStatus {
                    index,
                    message: format!("SOCKS listener stopped: {error}"),
                    active: false,
                });
                break;
            }
        }
    }
}

async fn handle_socks_connection(
    mut stream: TcpStream,
    forward_tx: UnboundedSender<ForwardRequest>,
) {
    let Some((target_host, target_port)) = socks5_request(&mut stream).await else {
        return;
    };

    let peer = stream.peer_addr().ok();
    let (reply, answer) = oneshot::channel();
    let request = ForwardRequest {
        target_host,
        target_port,
        origin_address: peer
            .map(|address| address.ip().to_string())
            .unwrap_or_else(|| "127.0.0.1".to_owned()),
        origin_port: peer.map(|address| u32::from(address.port())).unwrap_or(0),
        reply,
    };
    if forward_tx.send(request).is_err() {
        return;
    }

    match answer.await {
        Ok(Ok(channel)) => {
            // SOCKS5 success, BND.ADDR 0.0.0.0:0
            if stream
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .is_ok()
            {
                bridge(channel, stream).await;
            }
        }
        _ => {
            let _ = socks_reply(&mut stream, 1).await;
        }
    }
}

async fn socks5_request(stream: &mut TcpStream) -> Option<(String, u16)> {
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).await.ok()?;
    if greeting[0] != 5 {
        return None;
    }

    let mut methods = vec![0u8; usize::from(greeting[1])];
    stream.read_exact(&mut methods).await.ok()?;
    if !methods.contains(&0) {
        let _ = stream.write_all(&[5, 0xff]).await;
        return None;
    }
    stream.write_all(&[5, 0]).await.ok()?;

    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await.ok()?;
    if request[0] != 5 {
        return None;
    }
    if request[1] != 1 {
        let _ = socks_reply(stream, 7).await;
        return None;
    }

    let host = match request[3] {
        1 => {
            let mut bytes = [0u8; 4];
            stream.read_exact(&mut bytes).await.ok()?;
            Ipv4Addr::from(bytes).to_string()
        }
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await.ok()?;
            let mut bytes = vec![0u8; usize::from(length[0])];
            stream.read_exact(&mut bytes).await.ok()?;
            let host = String::from_utf8_lossy(&bytes).into_owned();
            if host.is_empty() {
                let _ = socks_reply(stream, 8).await;
                return None;
            }
            host
        }
        4 => {
            let mut bytes = [0u8; 16];
            stream.read_exact(&mut bytes).await.ok()?;
            Ipv6Addr::from(bytes).to_string()
        }
        _ => {
            let _ = socks_reply(stream, 8).await;
            return None;
        }
    };

    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await.ok()?;
    let port = u16::from_be_bytes(port);
    if port == 0 {
        let _ = socks_reply(stream, 1).await;
        return None;
    }
    Some((host, port))
}

async fn socks_reply(stream: &mut TcpStream, code: u8) -> std::io::Result<()> {
    stream.write_all(&[5, code, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

async fn bridge(mut channel: Channel<client::Msg>, mut stream: TcpStream) {
    let mut buffer = vec![0u8; 32 * 1024];
    let mut stream_open = true;

    loop {
        tokio::select! {
            read = stream.read(&mut buffer), if stream_open => {
                match read {
                    Ok(0) => {
                        stream_open = false;
                        let _ = channel.eof().await;
                    }
                    Ok(read) => {
                        if channel.data(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            message = channel.wait() => {
                match message {
                    Some(ChannelMsg::Data { data }) => {
                        if stream.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if stream.write_all(&data).await.is_err() {
                            break;
                        }
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                        let _ = stream.shutdown().await;
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let _ = stream.shutdown().await;
    let _ = channel.close().await;
}

async fn authenticate(
    session: &mut client::Handle<ClientHandler>,
    options: &ConnectOptions,
) -> Result<bool, String> {
    match options.auth {
        AuthMethod::Password => {
            let result = session
                .authenticate_password(&options.username, &options.password)
                .await
                .map_err(|error| format!("Authentication error: {error}"))?;
            if result.success() {
                Ok(true)
            } else {
                keyboard_interactive(session, options).await
            }
        }
        AuthMethod::PublicKey => {
            let passphrase =
                (!options.passphrase.is_empty()).then_some(options.passphrase.as_str());
            let key = load_secret_key(Path::new(&options.key_path), passphrase)
                .map_err(|error| format!("Could not load private key: {error}"))?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|error| format!("SSH negotiation failed: {error}"))?
                .flatten();
            let result = session
                .authenticate_publickey(
                    &options.username,
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
                .map_err(|error| format!("Authentication error: {error}"))?;
            Ok(result.success())
        }
    }
}

async fn keyboard_interactive(
    session: &mut client::Handle<ClientHandler>,
    options: &ConnectOptions,
) -> Result<bool, String> {
    use client::KeyboardInteractiveAuthResponse as Response;

    let mut response = match session
        .authenticate_keyboard_interactive_start(&options.username, None::<String>)
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(false),
    };

    loop {
        match response {
            Response::Success => return Ok(true),
            Response::Failure { .. } => return Ok(false),
            Response::InfoRequest { prompts, name, .. } => {
                let answers = prompts
                    .iter()
                    .map(|prompt| {
                        // Some servers ask a separate OTP question; reuse the
                        // password for password prompts, and leave others blank.
                        if prompt.echo {
                            String::new()
                        } else {
                            let _ = name;
                            options.password.clone()
                        }
                    })
                    .collect();
                response = session
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(|error| format!("Authentication error: {error}"))?;
            }
        }
    }
}
