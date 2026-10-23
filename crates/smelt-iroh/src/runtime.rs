//! 运行时链路：客户端经 iroh 直接说运行时的协议（设计文档 D2）。
//!
//! 一条 QUIC 双向流就是一条连接，和 unix socket 上的一条连接等价。两端都把流桥接成
//! `UnixStream::pair()` 的一端：运行时把另一端交给现有分发器，客户端把它交给
//! `RuntimeClient`。上层的阻塞读写、超时、`try_clone` 对两种传输完全一样。
//!
//! 鉴权不在这里：服务端把对端的 endpoint id 连同流一起交出去，由运行时按已配对设备
//! 判断（D3）。这里只按运行时给的「是否已配对」限流：endpoint id 不是秘密，谁都能连上来
//! 握手，未配对的对端不能靠多开连接、多开流把运行时的线程耗尽。

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{info, warn};

use crate::{ConnectionEvent, ConnectionObserver, PathObserver, RelaySettings};

/// 运行时协议的 ALPN。协议语义变了就换版本号，两端对不上 iroh 直接拒绝握手。
pub const RUNTIME_ALPN: &[u8] = b"smelt/runtime/1";

/// 同时最多留几条未配对的连接。配对只要一条，多出来的直接拒绝。
const MAX_UNTRUSTED_CONNECTIONS: usize = 8;
/// 未配对连接超限时的关闭码。
const CLOSE_BUSY: u32 = 2;
/// 未配对的连接最多留多久。到点还没配对就断开，名额不能被一条不说话的流一直占着；
/// 客户端下次请求时会重拨。
#[cfg(not(test))]
const UNTRUSTED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
#[cfg(test)]
const UNTRUSTED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
/// 未配对连接到期时的关闭码。
const CLOSE_PAIRING_TIMEOUT: u32 = 3;

/// 运行时一侧接收桥接好的流。
pub trait StreamAcceptor: Send + Sync + 'static {
    /// 对端是不是已配对的设备。可以阻塞（查库），在 blocking 线程池上调用。
    /// 未配对的连接受限：总数有上限，流一条处理完才接下一条。
    fn is_trusted(&self, remote: &str) -> bool;

    /// `remote` 是对端的 endpoint id。在 tokio 线程上被调用，不能阻塞：处理放到自己的线程。
    fn accept(&self, remote: &str, stream: UnixStream);
}

/// 活着的连接，按对端 endpoint id 分组。撤销设备时据此立即断开它的全部连接。
#[derive(Clone, Default)]
pub struct ConnectionRegistry {
    inner: Arc<Mutex<HashMap<String, Vec<Connection>>>>,
}

impl ConnectionRegistry {
    fn insert(&self, remote: &str, conn: Connection) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.entry(remote.to_string()).or_default().push(conn);
    }

    fn remove(&self, remote: &str, conn: &Connection) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conns) = inner.get_mut(remote) {
            conns.retain(|existing| existing.stable_id() != conn.stable_id());
            if conns.is_empty() {
                inner.remove(remote);
            }
        }
    }

    /// 断开某台设备的全部连接。返回断开了几条。
    pub fn close(&self, remote: &str) -> usize {
        let conns = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(remote)
            .unwrap_or_default();
        for conn in &conns {
            conn.close(1u32.into(), b"revoked");
        }
        conns.len()
    }

    /// 当前连着的设备。
    pub fn connected(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

/// 用户配置了自建 relay 就只用它（不做公共地址发现）；没配就用 n0 的公共 relay 和 DNS
/// 地址发现，只凭 endpoint id 就能找到对方，地址变了也能重新找到。
pub async fn bind_runtime_endpoint(
    secret: SecretKey,
    alpns: Vec<Vec<u8>>,
    relay: Option<&RelaySettings>,
) -> Result<Endpoint> {
    match relay {
        Some(relay) => crate::bind_endpoint(secret, alpns, relay).await,
        None => Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
            .alpns(alpns)
            .bind()
            .await
            .context("iroh Endpoint 绑定失败"),
    }
}

/// 接收运行时连接直到 `shutdown` 完成。每条双向流桥接后交给 `acceptor`。
pub async fn serve_runtime(
    endpoint: Endpoint,
    acceptor: Arc<dyn StreamAcceptor>,
    registry: ConnectionRegistry,
    shutdown: impl std::future::Future<Output = ()> + Send,
    path_observer: Option<PathObserver>,
    conn_observer: Option<ConnectionObserver>,
) {
    tokio::pin!(shutdown);
    let untrusted = Arc::new(Semaphore::new(MAX_UNTRUSTED_CONNECTIONS));
    loop {
        let incoming = tokio::select! {
            _ = &mut shutdown => break,
            incoming = endpoint.accept() => match incoming {
                Some(incoming) => incoming,
                None => break,
            },
        };
        let acceptor = Arc::clone(&acceptor);
        let registry = registry.clone();
        let path_observer = path_observer.clone();
        let conn_observer = conn_observer.clone();
        let untrusted = Arc::clone(&untrusted);
        tokio::spawn(async move {
            if let Err(error) = serve_connection(
                incoming,
                acceptor,
                untrusted,
                registry,
                path_observer,
                conn_observer,
            )
            .await
            {
                warn!("iroh 运行时连接处理失败：{error:#}");
            }
        });
    }
    endpoint.close().await;
}

async fn serve_connection(
    incoming: iroh::endpoint::Incoming,
    acceptor: Arc<dyn StreamAcceptor>,
    untrusted: Arc<Semaphore>,
    registry: ConnectionRegistry,
    path_observer: Option<PathObserver>,
    conn_observer: Option<ConnectionObserver>,
) -> Result<()> {
    let conn = incoming.await.context("握手失败")?;
    let connection_id = conn.stable_id();
    let remote = conn.remote_id().to_string();
    // 先登记再查白名单：撤销是先删白名单再按登记断连，这样查到「已配对」之后的撤销
    // 一定断得到这条连接，不存在「已配对的连接又变回未配对」。
    registry.insert(&remote, conn.clone());
    // 未配对期间占一个名额；在这条连接上配对成功后释放。
    let mut untrusted_slot: Option<OwnedSemaphorePermit> = None;
    if !is_trusted(&acceptor, &remote).await {
        match untrusted.try_acquire_owned() {
            Ok(permit) => untrusted_slot = Some(permit),
            Err(_) => {
                warn!(%remote, "未配对的连接已达上限，拒绝");
                registry.remove(&remote, &conn);
                conn.close(CLOSE_BUSY.into(), b"busy");
                return Ok(());
            }
        }
    }
    let deadline = tokio::time::Instant::now() + UNTRUSTED_DEADLINE;
    info!(%remote, "iroh 已接受运行时连接");
    if let Some(observer) = conn_observer.as_ref() {
        let connected_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        observer(ConnectionEvent::Connected {
            connection_id,
            remote_id: remote.clone(),
            connected_at,
        });
    }
    crate::observe_selected_paths(conn.clone(), remote.clone(), path_observer);

    loop {
        let limit = untrusted_slot.as_ref().map(|_| deadline);
        let Some(accepted) = before(limit, conn.accept_bi()).await else {
            expire(&conn, &remote);
            break;
        };
        let (send, recv) = match accepted {
            Ok(pair) => pair,
            Err(error) => {
                info!(%remote, "iroh 运行时连接结束：{error}");
                break;
            }
        };
        if untrusted_slot.is_some() && is_trusted(&acceptor, &remote).await {
            untrusted_slot = None;
        }
        match bridge_tracked(send, recv) {
            Ok((stream, pumped)) => {
                acceptor.accept(&remote, stream);
                // 未配对的流一条处理完再接下一条：对端多开的流留在 QUIC 层，受它的流控约束，
                // 不会各占一个处理线程。
                if untrusted_slot.is_some() && before(Some(deadline), pumped).await.is_none() {
                    expire(&conn, &remote);
                    break;
                }
            }
            Err(error) => warn!(%remote, "桥接 iroh 流失败：{error}"),
        }
    }

    registry.remove(&remote, &conn);
    if let Some(observer) = conn_observer {
        observer(ConnectionEvent::Disconnected {
            connection_id,
            remote_id: remote,
        });
    }
    Ok(())
}

/// 在 `deadline` 之前等完 `fut`；`deadline` 为 `None` 时不限时，到点返回 `None`。
async fn before<F: std::future::Future>(
    deadline: Option<tokio::time::Instant>,
    fut: F,
) -> Option<F::Output> {
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, fut).await.ok(),
        None => Some(fut.await),
    }
}

/// 未配对的连接到期：断开。搬运任务随之出错收摊，处理线程读到 EOF 退出。
fn expire(conn: &Connection, remote: &str) {
    warn!(%remote, "未配对的连接到期，断开");
    conn.close(CLOSE_PAIRING_TIMEOUT.into(), b"pairing timeout");
}

async fn is_trusted(acceptor: &Arc<dyn StreamAcceptor>, remote: &str) -> bool {
    let acceptor = Arc::clone(acceptor);
    let remote = remote.to_string();
    tokio::task::spawn_blocking(move || acceptor.is_trusted(&remote))
        .await
        .unwrap_or(false)
}

/// 把一条 QUIC 双向流接到 `UnixStream::pair()` 的一端，返回另一端。必须在 tokio
/// 运行时里调用：搬运任务挂在当前运行时上。
pub fn bridge(send: SendStream, recv: RecvStream) -> io::Result<UnixStream> {
    bridge_tracked(send, recv).map(|(stream, _)| stream)
}

/// 同 [`bridge`]，另返回搬运任务：它结束时这条流已经收摊。
fn bridge_tracked(
    send: SendStream,
    recv: RecvStream,
) -> io::Result<(UnixStream, tokio::task::JoinHandle<()>)> {
    let (ours, theirs) = UnixStream::pair()?;
    ours.set_nonblocking(true)?;
    let ours = tokio::net::UnixStream::from_std(ours)?;
    Ok((theirs, tokio::spawn(pump(send, recv, ours))))
}

/// 两个方向分别搬到对端 EOF。对端只是写完（半关闭）时，另一方向继续，回包还要回去；
/// 任一方向出错，或本地一端关闭，就整条收摊，避免长流在对端消失后挂着。
async fn pump(mut send: SendStream, mut recv: RecvStream, unix: tokio::net::UnixStream) {
    let (mut unix_read, mut unix_write) = unix.into_split();
    let up = async {
        tokio::io::copy(&mut recv, &mut unix_write).await?;
        unix_write.shutdown().await
    };
    let down = async {
        tokio::io::copy(&mut unix_read, &mut send).await?;
        send.finish().map_err(io::Error::other)
    };
    tokio::pin!(up, down);
    let mut up_done = false;
    loop {
        tokio::select! {
            result = &mut up, if !up_done => match result {
                Ok(()) => up_done = true,
                Err(_) => return,
            },
            _ = &mut down => return,
        }
    }
}

/// 拨号一个运行时最多等多久。离线的运行时靠地址发现找不到，不能把调用方一直挂着。
const DIAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// 客户端这一侧的 iroh endpoint，所有远程运行时共用一个：运行时按客户端密钥的公钥认
/// 设备，同一把密钥同时绑几个 endpoint 会让 relay 上出现重复的节点。
///
/// 不给 relay 时用 n0 的公共 relay 和地址发现；目标运行时用了自建 relay 时，拨号地址里
/// 带上它即可（桌面就是这样，一个客户端连几台运行时）。给了 relay 时只用它，域名交给系统
/// 解析器：手机只连一台运行时，而 iOS 上 iroh 自带的 DNS 会绕开系统代理，见
/// [`crate::system_dns`]。
pub struct IrohClient {
    runtime: tokio::runtime::Runtime,
    endpoint: Endpoint,
}

impl IrohClient {
    /// `secret` 是这台客户端自己的身份，必须持久。
    pub fn new(secret: SecretKey, relay: Option<&RelaySettings>) -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("smelt-iroh-client")
            .enable_all()
            .build()
            .context("起不了 iroh 客户端的 tokio 运行时")?;
        let endpoint = runtime.block_on(bind_runtime_endpoint(secret, Vec::new(), relay))?;
        Ok(Arc::new(Self { runtime, endpoint }))
    }

    /// 这台客户端的设备身份（endpoint id）。
    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// 连某个运行时的连接器。
    pub fn connector(
        self: &Arc<Self>,
        target: EndpointId,
        relay: Option<&RelaySettings>,
    ) -> RuntimeConnector {
        let mut addr = EndpointAddr::new(target);
        if let Some(relay) = relay {
            addr = addr.with_relay_url(relay.url.clone());
        }
        RuntimeConnector {
            client: Arc::clone(self),
            target: addr,
            conn: tokio::sync::Mutex::new(None),
        }
    }
}

impl Drop for IrohClient {
    fn drop(&mut self) {
        let endpoint = self.endpoint.clone();
        self.runtime.block_on(async move { endpoint.close().await });
    }
}

/// 经 iroh 连一个远程运行时。实现 [`smelt_runtime_client::Connector`]，每次 `connect`
/// 在同一条 QUIC 连接上新开一条流；连接断了就重拨。不能在 tokio 运行时里面调用。
pub struct RuntimeConnector {
    client: Arc<IrohClient>,
    target: EndpointAddr,
    conn: tokio::sync::Mutex<Option<Connection>>,
}

impl RuntimeConnector {
    async fn connection(&self) -> io::Result<Connection> {
        let mut slot = self.conn.lock().await;
        if let Some(conn) = slot.as_ref()
            && conn.close_reason().is_none()
        {
            return Ok(conn.clone());
        }
        let dial = self
            .client
            .endpoint
            .connect(self.target.clone(), RUNTIME_ALPN);
        let conn = tokio::time::timeout(DIAL_TIMEOUT, dial)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "连不上运行时（超时）"))?
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionRefused, error))?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    async fn forget(&self, failed: &Connection) {
        let mut slot = self.conn.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| current.stable_id() == failed.stable_id())
        {
            *slot = None;
        }
    }
}

impl RuntimeConnector {
    /// 当前连接选中的传输路径和 RTT。还没连上、连接已断或还没选出路径时为 None。
    /// 不会为此拨号。和 [`smelt_runtime_client::Connector::connect`] 一样不能在 tokio
    /// 运行时里调用。
    pub fn path_status(&self) -> Option<crate::PathStatus> {
        self.client.runtime.block_on(async {
            let conn = self.conn.lock().await.clone()?;
            if conn.close_reason().is_some() {
                return None;
            }
            crate::selected_path(&conn, &self.target.id.to_string())
        })
    }
}

impl smelt_runtime_client::Connector for RuntimeConnector {
    fn connect(&self) -> io::Result<UnixStream> {
        self.client.runtime.block_on(async {
            let conn = self.connection().await?;
            match conn.open_bi().await {
                Ok((send, recv)) => bridge(send, recv),
                // 网络切换后旧连接可能还没察觉自己死了：丢掉重拨一次。
                Err(_) => {
                    self.forget(&conn).await;
                    let conn = self.connection().await?;
                    let (send, recv) = conn.open_bi().await.map_err(io::Error::other)?;
                    bridge(send, recv)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use iroh::RelayMode;
    use iroh::endpoint::ConnectionError;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    const TEST_ALPN: &[u8] = b"smelt/test/1";
    const ARRIVES: Duration = Duration::from_secs(5);
    const STAYS_AWAY: Duration = Duration::from_millis(300);

    struct TestAcceptor {
        trusted: AtomicBool,
        streams: mpsc::UnboundedSender<UnixStream>,
    }

    impl StreamAcceptor for TestAcceptor {
        fn is_trusted(&self, _remote: &str) -> bool {
            self.trusted.load(Ordering::SeqCst)
        }

        fn accept(&self, _remote: &str, stream: UnixStream) {
            let _ = self.streams.send(stream);
        }
    }

    async fn local_endpoint(alpns: Vec<Vec<u8>>) -> Endpoint {
        Endpoint::builder(iroh::endpoint::presets::Minimal)
            .secret_key(SecretKey::generate())
            .alpns(alpns)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .unwrap()
    }

    /// 起一个运行时一侧的 endpoint；返回它的地址、acceptor 和它收到的流。
    async fn serve(
        trusted: bool,
    ) -> (
        EndpointAddr,
        Arc<TestAcceptor>,
        mpsc::UnboundedReceiver<UnixStream>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let acceptor = Arc::new(TestAcceptor {
            trusted: AtomicBool::new(trusted),
            streams: tx,
        });
        let endpoint = local_endpoint(vec![TEST_ALPN.to_vec()]).await;
        let addr = endpoint.addr();
        tokio::spawn(serve_runtime(
            endpoint,
            Arc::clone(&acceptor) as Arc<dyn StreamAcceptor>,
            ConnectionRegistry::default(),
            std::future::pending(),
            None,
            None,
        ));
        (addr, acceptor, rx)
    }

    async fn connect(client: &Endpoint, addr: &EndpointAddr) -> Connection {
        client.connect(addr.clone(), TEST_ALPN).await.unwrap()
    }

    /// 开一条流并写一个字节：对端要收到数据才看得见这条流。
    async fn open(conn: &Connection) -> (SendStream, RecvStream) {
        let (mut send, recv) = conn.open_bi().await.unwrap();
        send.write_all(b"x").await.unwrap();
        (send, recv)
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<UnixStream>) -> UnixStream {
        timeout(ARRIVES, rx.recv()).await.unwrap().unwrap()
    }

    async fn nothing_more(rx: &mut mpsc::UnboundedReceiver<UnixStream>) {
        assert!(timeout(STAYS_AWAY, rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn untrusted_streams_are_handled_one_at_a_time() {
        let (addr, _acceptor, mut rx) = serve(false).await;
        let client = local_endpoint(Vec::new()).await;
        let conn = connect(&client, &addr).await;
        let _first = open(&conn).await;
        let _second = open(&conn).await;

        let first = next(&mut rx).await;
        nothing_more(&mut rx).await;
        drop(first);
        next(&mut rx).await;
    }

    #[tokio::test]
    async fn trusted_streams_are_handled_concurrently() {
        let (addr, _acceptor, mut rx) = serve(true).await;
        let client = local_endpoint(Vec::new()).await;
        let conn = connect(&client, &addr).await;
        let _first = open(&conn).await;
        let _second = open(&conn).await;

        let _held = next(&mut rx).await;
        next(&mut rx).await;
    }

    #[tokio::test]
    async fn pairing_on_the_connection_lifts_the_limit() {
        let (addr, acceptor, mut rx) = serve(false).await;
        let client = local_endpoint(Vec::new()).await;
        let conn = connect(&client, &addr).await;
        let _pair = open(&conn).await;
        let pair = next(&mut rx).await;
        acceptor.trusted.store(true, Ordering::SeqCst);
        drop(pair);

        let _second = open(&conn).await;
        let _third = open(&conn).await;
        let _held = next(&mut rx).await;
        next(&mut rx).await;
    }

    #[tokio::test]
    async fn untrusted_connections_beyond_the_cap_are_refused() {
        let (addr, _acceptor, _rx) = serve(false).await;
        let client = local_endpoint(Vec::new()).await;
        let mut conns = Vec::new();
        for _ in 0..=MAX_UNTRUSTED_CONNECTIONS {
            conns.push(connect(&client, &addr).await);
        }

        let mut refused = 0;
        for conn in &conns {
            if let Ok(ConnectionError::ApplicationClosed(close)) =
                timeout(STAYS_AWAY, conn.closed()).await
                && close.error_code == CLOSE_BUSY.into()
            {
                refused += 1;
            }
        }
        assert_eq!(refused, 1);
    }

    /// 处理线程卡着不收摊（对端只写半行就不说话）：到期整条连接断开，名额让出来。
    #[tokio::test]
    async fn stalled_untrusted_connections_expire_and_free_their_slot() {
        let (addr, _acceptor, mut rx) = serve(false).await;
        let client = local_endpoint(Vec::new()).await;
        let mut conns = Vec::new();
        let mut opened = Vec::new();
        let mut stuck = Vec::new();
        for _ in 0..MAX_UNTRUSTED_CONNECTIONS {
            let conn = connect(&client, &addr).await;
            opened.push(open(&conn).await);
            stuck.push(next(&mut rx).await);
            conns.push(conn);
        }

        let extra = connect(&client, &addr).await;
        match timeout(STAYS_AWAY, extra.closed()).await {
            Ok(ConnectionError::ApplicationClosed(close)) => {
                assert_eq!(close.error_code, CLOSE_BUSY.into());
            }
            other => panic!("名额满时应拒绝：{other:?}"),
        }

        for conn in &conns {
            match timeout(UNTRUSTED_DEADLINE + ARRIVES, conn.closed()).await {
                Ok(ConnectionError::ApplicationClosed(close)) => {
                    assert_eq!(close.error_code, CLOSE_PAIRING_TIMEOUT.into());
                }
                other => panic!("到期应断开：{other:?}"),
            }
        }

        let later = connect(&client, &addr).await;
        let _later = open(&later).await;
        next(&mut rx).await;
    }
}
