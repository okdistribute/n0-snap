use crate::model::{Invite, Offer, Ticket, now, private_write};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{Connection, presets},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};

pub const STORE_ALPN: &[u8] = b"flicker-experiment/store/1";
pub const INBOX_ALPN: &[u8] = b"flicker-experiment/inbox/1";
pub const FRIEND_ALPN: &[u8] = b"flicker-experiment/friend/1";
pub const MAX_MEDIA: usize = 12 * 1024 * 1024;
const MAX_FRAME: usize = 18 * 1024 * 1024;
const MAX_STORE_BYTES: u64 = 512 * 1024 * 1024;

pub fn random_hex() -> String {
    let mut x = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut x);
    hex::encode(x)
}
fn digest(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
fn matches_secret(a: &str, b: &str) -> bool {
    let a = Sha256::digest(a.as_bytes());
    let b = Sha256::digest(b.as_bytes());
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub fn secret(path: &std::path::Path) -> Result<SecretKey> {
    if path.exists() {
        let bytes: [u8; 32] = std::fs::read(path)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid endpoint key"))?;
        Ok(SecretKey::from_bytes(&bytes))
    } else {
        let key = SecretKey::generate();
        private_write(path, &key.to_bytes())?;
        Ok(key)
    }
}

#[derive(Serialize, Deserialize)]
enum Request {
    Put {
        token: String,
        id: String,
        read_hash: String,
        expires_at: u64,
        data: String,
    },
    Get {
        id: String,
        read_cap: String,
    },
    Ping {
        token: String,
    },
}
#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    data: Option<String>,
    error: Option<String>,
}
impl Response {
    fn ok(data: Option<String>) -> Self {
        Self {
            ok: true,
            data,
            error: None,
        }
    }
    fn error(e: impl ToString) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(e.to_string()),
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Stored {
    read_hash: String,
    expires_at: u64,
    data: String,
}

pub struct Store {
    pub endpoint: Endpoint,
    pub token: String,
}

pub async fn start_store(dir: PathBuf, token: String) -> Result<Store> {
    ensure!(
        token.len() >= 24,
        "Storage write token must contain at least 24 characters"
    );
    std::fs::create_dir_all(dir.join("objects"))?;
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret(&dir.join("endpoint.key"))?)
        .alpns(vec![STORE_ALPN.to_vec()])
        .bind()
        .await?;
    let quota = Arc::new(Mutex::new(()));
    let limit = Arc::new(Semaphore::new(16));
    let accept_ep = endpoint.clone();
    let auth = token.clone();
    let root = dir.clone();
    let accept_quota = quota.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accept_ep.accept().await {
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            let dir = root.clone();
            let token = auth.clone();
            let quota = accept_quota.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(Duration::from_secs(40), async {
                    let conn = incoming.await?;
                    let (mut send, mut recv) = conn.accept_bi().await?;
                    let response = match recv.read_to_end(MAX_FRAME).await {
                        Ok(bytes) => match serde_json::from_slice::<Request>(&bytes) {
                            Ok(req) => match handle_store(req, &dir, &token, &quota).await {
                                Ok(r) => r,
                                Err(e) => Response::error(e),
                            },
                            Err(_) => Response::error("Invalid request"),
                        },
                        Err(_) => Response::error("Request too large"),
                    };
                    send.write_all(&serde_json::to_vec(&response)?).await?;
                    send.finish()?;
                    let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
                    anyhow::Ok(())
                })
                .await;
            });
        }
    });
    let cleanup_ep = endpoint.clone();
    tokio::spawn(async move {
        while !cleanup_ep.is_closed() {
            let _lock = quota.lock().await;
            let _ = prune(&dir).await;
            drop(_lock);
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    Ok(Store { endpoint, token })
}

async fn prune(dir: &std::path::Path) -> Result<u64> {
    let mut entries = tokio::fs::read_dir(dir.join("objects")).await?;
    let mut bytes = 0;
    while let Some(entry) = entries.next_entry().await? {
        if entry.path().extension().is_none_or(|s| s != "json") {
            continue;
        }
        let data = tokio::fs::read(entry.path()).await?;
        match serde_json::from_slice::<Stored>(&data) {
            Ok(record) if record.expires_at > now() => bytes += data.len() as u64,
            _ => {
                tokio::fs::remove_file(entry.path()).await?;
            }
        }
    }
    Ok(bytes)
}

async fn handle_store(
    req: Request,
    dir: &std::path::Path,
    token: &str,
    quota: &Mutex<()>,
) -> Result<Response> {
    match req {
        Request::Ping { token: provided } => {
            ensure!(
                matches_secret(token, &provided),
                "Storage token was rejected"
            );
            Ok(Response::ok(None))
        }
        Request::Put {
            token: provided,
            id,
            read_hash,
            expires_at,
            data,
        } => {
            ensure!(
                matches_secret(token, &provided),
                "Storage token was rejected"
            );
            let id = uuid::Uuid::parse_str(&id)?;
            ensure!(
                read_hash.len() == 64 && read_hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid read capability"
            );
            ensure!(
                expires_at > now() && expires_at <= now() + 86400,
                "Storage lifetime must be within 24 hours"
            );
            ensure!(
                data.len() <= ((MAX_MEDIA + 16) * 4 / 3 + 8),
                "Media exceeds 12 MB"
            );
            let _lock = quota.lock().await;
            let current = prune(dir).await?;
            ensure!(
                current + (data.len() as u64) <= MAX_STORE_BYTES,
                "Storage is full (512 MB prototype quota)"
            );
            let path = dir.join("objects").join(format!("{id}.json"));
            ensure!(!path.exists(), "Object already exists");
            let record = serde_json::to_vec(&Stored {
                read_hash,
                expires_at,
                data,
            })?;
            let tmp = path.with_extension("tmp");
            tokio::fs::write(&tmp, record).await?;
            tokio::fs::rename(tmp, path).await?;
            Ok(Response::ok(None))
        }
        Request::Get { id, read_cap } => {
            let id = uuid::Uuid::parse_str(&id)?;
            let record: Stored = serde_json::from_slice(
                &tokio::fs::read(dir.join("objects").join(format!("{id}.json")))
                    .await
                    .context("Snap is no longer on this host")?,
            )?;
            ensure!(
                matches_secret(&record.read_hash, &digest(&read_cap)),
                "Invalid read capability"
            );
            ensure!(
                record.expires_at > now(),
                "This snap has expired on its host"
            );
            Ok(Response::ok(Some(record.data)))
        }
    }
}

async fn request(endpoint: &Endpoint, addr: EndpointAddr, req: Request) -> Result<Response> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let conn = endpoint.connect(addr, STORE_ALPN).await?;
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&serde_json::to_vec(&req)?).await?;
        send.finish()?;
        let bytes = recv.read_to_end(MAX_FRAME).await?;
        conn.close(0u32.into(), b"done");
        let response: Response = serde_json::from_slice(&bytes)?;
        ensure!(
            response.ok,
            "{}",
            response
                .error
                .as_deref()
                .unwrap_or("Storage request failed")
        );
        Ok(response)
    })
    .await
    .context("Storage endpoint did not respond within 30 seconds")?
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FriendStatus {
    Pending,
    Accepted,
    Declined,
}

#[derive(Serialize, Deserialize)]
struct FriendHello {
    token: String,
    invite: Invite,
}

#[cfg(feature = "app")]
#[derive(Clone)]
pub struct AccountPeers {
    pub own: Arc<RwLock<Option<String>>>,
    pub bindings: Arc<RwLock<std::collections::HashMap<iroh::EndpointId, String>>>,
    directory: Arc<dyn crate::devices::DeviceDirectory>,
}
#[cfg(feature = "app")]
impl AccountPeers {
    async fn verify_pair(
        &self,
        local: iroh::EndpointId,
        remote: iroh::EndpointId,
        did: &str,
    ) -> Result<()> {
        let own = self
            .own
            .read()
            .await
            .clone()
            .context("Sign in to connect by account")?;
        tokio::try_join!(
            self.directory.check(&own, local),
            self.directory.check(did, remote)
        )?;
        Ok(())
    }
    async fn verify_media(&self, local: iroh::EndpointId, remote: iroh::EndpointId) -> Result<()> {
        let did = self.bindings.read().await.get(&remote).cloned();
        if let Some(did) = did {
            self.verify_pair(local, remote, &did).await?;
        }
        Ok(())
    }
}

pub struct Client {
    pub endpoint: Endpoint,
    pub local_store: Store,
    pub allowed: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    pub declined: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    pub request_token: String,
    #[cfg(feature = "app")]
    pub accounts: AccountPeers,
}

impl Client {
    pub async fn start(dir: PathBuf, tx: mpsc::Sender<(iroh::EndpointId, Offer)>) -> Result<Self> {
        let (friend_tx, _) = mpsc::channel(1);
        Self::start_with_requests(dir, tx, friend_tx).await
    }
    pub async fn start_with_requests(
        dir: PathBuf,
        tx: mpsc::Sender<(iroh::EndpointId, Offer)>,
        friend_tx: mpsc::Sender<Invite>,
    ) -> Result<Self> {
        Self::start_inner(
            dir,
            tx,
            friend_tx,
            #[cfg(feature = "app")]
            Arc::new(crate::devices::Directory::new()?),
        )
        .await
    }
    async fn start_inner(
        dir: PathBuf,
        tx: mpsc::Sender<(iroh::EndpointId, Offer)>,
        friend_tx: mpsc::Sender<Invite>,
        #[cfg(feature = "app")] directory: Arc<dyn crate::devices::DeviceDirectory>,
    ) -> Result<Self> {
        let request_path = dir.join("friend-request.token");
        let request_token = if request_path.exists() {
            std::fs::read_to_string(&request_path)?
        } else {
            let token = random_hex();
            private_write(&request_path, token.as_bytes())?;
            token
        };
        let token_path = dir.join("local-store.token");
        let token = if token_path.exists() {
            std::fs::read_to_string(&token_path)?
        } else {
            let t = random_hex();
            private_write(&token_path, t.as_bytes())?;
            t
        };
        let local_store = start_store(dir.join("host"), token).await?;
        #[allow(unused_mut)]
        let mut alpns = vec![INBOX_ALPN.to_vec(), FRIEND_ALPN.to_vec()];
        #[cfg(feature = "app")]
        alpns.push(crate::devices::FRIEND_ALPN.to_vec());
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret(&dir.join("device.key"))?)
            .alpns(alpns)
            .bind()
            .await?;
        let allowed = Arc::new(RwLock::new(HashSet::new()));
        let ep = endpoint.clone();
        let peers = allowed.clone();
        let declined = Arc::new(RwLock::new(HashSet::new()));
        let rejected = declined.clone();
        let seen = Arc::new(Mutex::new(HashSet::new()));
        let friend_token = request_token.clone();
        #[cfg(feature = "app")]
        let accounts = AccountPeers {
            own: Default::default(),
            bindings: Default::default(),
            directory,
        };
        #[cfg(feature = "app")]
        let account_peers = accounts.clone();
        tokio::spawn(async move {
            let limit = Arc::new(Semaphore::new(8));
            while let Some(incoming) = ep.accept().await {
                let Ok(permit) = limit.clone().try_acquire_owned() else {
                    incoming.refuse();
                    continue;
                };
                let peers = peers.clone();
                let tx = tx.clone();
                let friend_tx = friend_tx.clone();
                let friend_token = friend_token.clone();
                let rejected = rejected.clone();
                let seen = seen.clone();
                #[cfg(feature = "app")]
                let accounts = account_peers.clone();
                #[cfg(feature = "app")]
                let local = ep.id();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(Duration::from_secs(40), async {
                        let conn = incoming.await?;
                        #[cfg(feature = "app")]
                        if conn.alpn() == crate::devices::FRIEND_ALPN {
                            return accept_account_friend(
                                conn, local, accounts, peers, rejected, seen, friend_tx,
                            )
                            .await;
                        }
                        if conn.alpn() == FRIEND_ALPN {
                            accept_friend(conn, peers, rejected, seen, friend_token, friend_tx)
                                .await
                        } else {
                            #[cfg(feature = "app")]
                            accounts.verify_media(local, conn.remote_id()).await?;
                            accept_offer(conn, peers, tx).await
                        }
                    })
                    .await;
                });
            }
        });
        Ok(Self {
            endpoint,
            local_store,
            allowed,
            declined,
            request_token,
            #[cfg(feature = "app")]
            accounts,
        })
    }
    /// Returns true once the recipient explicitly accepts. Repeating a pending hello
    /// checks acceptance without granting permission to send media.
    pub async fn request_friend(&self, target: &Invite, own: Invite) -> Result<FriendStatus> {
        #[cfg(feature = "app")]
        if target.version == 2 {
            return self.request_account_friend(target, own).await;
        }
        ensure!(
            !target.request_token.is_empty(),
            "This older Snapcode needs exchanging codes on both devices."
        );
        tokio::time::timeout(Duration::from_secs(12), async {
            let conn = self
                .endpoint
                .connect(target.endpoint.clone(), FRIEND_ALPN)
                .await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(&serde_json::to_vec(&FriendHello {
                token: target.request_token.clone(),
                invite: own,
            })?)
            .await?;
            send.finish()?;
            let reply: Response = serde_json::from_slice(&recv.read_to_end(4096).await?)?;
            conn.close(0u32.into(), b"done");
            ensure!(
                reply.ok,
                "{}",
                reply
                    .error
                    .unwrap_or_else(|| "Friend request failed".into())
            );
            match reply.data.as_deref() {
                Some("accepted") => Ok(FriendStatus::Accepted),
                Some("pending") => Ok(FriendStatus::Pending),
                Some("declined") => Ok(FriendStatus::Declined),
                _ => anyhow::bail!("Invalid friend response"),
            }
        })
        .await
        .context("Your friend is offline. Open both apps and try again.")?
    }
    pub async fn probe(&self, addr: EndpointAddr, token: String) -> Result<()> {
        request(&self.endpoint, addr, Request::Ping { token }).await?;
        Ok(())
    }
    pub async fn upload(
        &self,
        bytes: &[u8],
        mime: &str,
        store: EndpointAddr,
        token: String,
    ) -> Result<Ticket> {
        ensure!(bytes.len() <= MAX_MEDIA, "Choose media smaller than 12 MB");
        ensure!(allowed_mime(mime), "Unsupported media format");
        let key = random_hex();
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let cipher = Aes256Gcm::new_from_slice(&hex::decode(&key)?)
            .map_err(|_| anyhow::anyhow!("Encryption key error"))?;
        let encrypted = cipher
            .encrypt(Nonce::from_slice(&nonce), bytes)
            .map_err(|_| anyhow::anyhow!("Could not encrypt media"))?;
        let ticket = Ticket {
            id: uuid::Uuid::new_v4().to_string(),
            store: store.clone(),
            read_cap: random_hex(),
            key,
            nonce: hex::encode(nonce),
            expires_at: now() + 86400,
            mime: mime.into(),
        };
        request(
            &self.endpoint,
            store,
            Request::Put {
                token,
                id: ticket.id.clone(),
                read_hash: digest(&ticket.read_cap),
                expires_at: ticket.expires_at,
                data: STANDARD.encode(encrypted),
            },
        )
        .await?;
        Ok(ticket)
    }
    pub async fn download_cipher(&self, ticket: &Ticket) -> Result<String> {
        ensure!(now() < ticket.expires_at, "This snap has expired");
        request(
            &self.endpoint,
            ticket.store.clone(),
            Request::Get {
                id: ticket.id.clone(),
                read_cap: ticket.read_cap.clone(),
            },
        )
        .await?
        .data
        .context("Host returned no media")
    }
    pub async fn send(&self, addr: EndpointAddr, offer: &Offer) -> Result<()> {
        #[cfg(feature = "app")]
        self.accounts
            .verify_media(self.endpoint.id(), addr.id)
            .await
            .context("Account device registration is missing, revoked, or could not be verified")?;
        tokio::time::timeout(Duration::from_secs(40), async {
            let conn = self.endpoint.connect(addr, INBOX_ALPN).await?;
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(&serde_json::to_vec(offer)?).await?;
            send.finish()?;
            let reply: Response = serde_json::from_slice(&recv.read_to_end(4096).await?)?;
            conn.close(0u32.into(), b"done");
            ensure!(
                reply.ok,
                "{}",
                reply
                    .error
                    .unwrap_or_else(|| "Friend rejected this snap".into())
            );
            Ok(())
        })
        .await
        .context("Friend is offline. Both apps must be open to deliver a new snap invitation.")?
    }
}

#[cfg(feature = "app")]
impl Client {
    pub async fn authorize_friend(&self, invite: &Invite) -> Result<()> {
        if invite.version == 2 {
            self.accounts
                .verify_pair(self.endpoint.id(), invite.endpoint.id, &invite.did)
                .await?;
            self.accounts
                .bindings
                .write()
                .await
                .insert(invite.endpoint.id, invite.did.clone());
        }
        self.allowed.write().await.insert(invite.endpoint.id);
        Ok(())
    }
    async fn request_account_friend(
        &self,
        target: &Invite,
        mut own: Invite,
    ) -> Result<FriendStatus> {
        self.accounts
            .verify_pair(self.endpoint.id(), target.endpoint.id, &target.did)
            .await
            .context("Both devices need current, verifiable PDS device records")?;
        own.version = 2;
        own.did = self
            .accounts
            .own
            .read()
            .await
            .clone()
            .context("Sign in first")?;
        own.request_token.clear();
        // Resolve routes through iroh's endpoint discovery, never through record-supplied addresses.
        own.endpoint = self.endpoint.id().into();
        tokio::time::timeout(Duration::from_secs(45), async {
            let conn = self
                .endpoint
                .connect(target.endpoint.id, crate::devices::FRIEND_ALPN)
                .await?;
            ensure!(
                conn.remote_id() == target.endpoint.id,
                "Connected device mismatch"
            );
            let (mut send, mut recv) = conn.open_bi().await?;
            send.write_all(&serde_json::to_vec(&FriendHello {
                token: String::new(),
                invite: own,
            })?)
            .await?;
            send.finish()?;
            let reply: Response = serde_json::from_slice(&recv.read_to_end(4096).await?)?;
            conn.close(0u32.into(), b"done");
            ensure!(
                reply.ok,
                "{}",
                reply
                    .error
                    .unwrap_or_else(|| "Account connection failed".into())
            );
            match reply.data.as_deref() {
                Some("accepted") => Ok(FriendStatus::Accepted),
                Some("pending") => Ok(FriendStatus::Pending),
                Some("declined") => Ok(FriendStatus::Declined),
                _ => anyhow::bail!("Invalid friend response"),
            }
        })
        .await
        .context("Your friend is offline. Keep both native apps open and retry.")?
    }
}

#[cfg(feature = "app")]
async fn accept_account_friend(
    conn: Connection,
    local: iroh::EndpointId,
    accounts: AccountPeers,
    allowed: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    declined: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    seen: Arc<Mutex<HashSet<iroh::EndpointId>>>,
    tx: mpsc::Sender<Invite>,
) -> Result<()> {
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let result: Result<&str> = async {
        let hello: FriendHello = serde_json::from_slice(&recv.read_to_end(16000).await?)?;
        ensure!(
            hello.invite.version == 2
                && hello.invite.endpoint.id == remote
                && hello.invite.did.len() <= 256
                && hello.invite.request_token.is_empty()
                && hello.token.is_empty(),
            "Invalid account invitation"
        );
        if declined.read().await.contains(&remote) {
            return Ok("declined");
        }
        accounts
            .verify_pair(local, remote, &hello.invite.did)
            .await?;
        if allowed.read().await.contains(&remote) {
            // Never silently rebind an existing account-linked device to another DID.
            ensure!(
                accounts
                    .bindings
                    .read()
                    .await
                    .get(&remote)
                    .is_none_or(|did| did == &hello.invite.did),
                "Device account changed; reconnect explicitly"
            );
            return Ok("accepted");
        }
        if seen.lock().await.contains(&remote) {
            return Ok("pending");
        }
        // Names are loaded from the claimed *verified* DID, not supplied by the peer.
        let profile = accounts.directory.profile(&hello.invite.did).await?;
        ensure!(profile.did == hello.invite.did, "Profile identity mismatch");
        let invite = Invite {
            version: 2,
            name: profile.label().chars().take(80).collect(),
            handle: profile.handle,
            did: hello.invite.did,
            endpoint: remote.into(),
            request_token: String::new(),
        };
        let mut seen = seen.lock().await;
        if !seen.contains(&remote) {
            ensure!(seen.len() < 128, "Too many pending requests");
            tx.try_send(invite)
                .context("Connection requests are busy")?;
            seen.insert(remote);
        }
        Ok("pending")
    }
    .await;
    let response = match result {
        Ok(status) => Response::ok(Some(status.into())),
        Err(e) => Response::error(e),
    };
    send.write_all(&serde_json::to_vec(&response)?).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    Ok(())
}

async fn accept_friend(
    conn: Connection,
    allowed: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    declined: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    seen: Arc<Mutex<HashSet<iroh::EndpointId>>>,
    token: String,
    tx: mpsc::Sender<Invite>,
) -> Result<()> {
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let result: Result<FriendStatus> = async {
        let hello: FriendHello = serde_json::from_slice(&recv.read_to_end(16000).await?)?;
        ensure!(
            matches_secret(&token, &hello.token),
            "Invalid Snapcode permission"
        );
        ensure!(
            hello.invite.endpoint.id == remote,
            "Invitation does not match connected device"
        );
        ensure!(
            hello.invite.version == 1
                && hello.invite.name.chars().count() <= 80
                && hello.invite.name.len() <= 320
                && hello.invite.handle.len() < 256
                && hello.invite.request_token.len() <= 128,
            "Invalid invitation"
        );
        if declined.read().await.contains(&remote) {
            return Ok(FriendStatus::Declined);
        }
        if allowed.read().await.contains(&remote) {
            return Ok(FriendStatus::Accepted);
        }
        let mut seen = seen.lock().await;
        if !seen.contains(&remote) {
            ensure!(seen.len() < 128, "Friend request inbox is full");
            tx.try_send(hello.invite)
                .context("Friend request inbox is busy")?;
            seen.insert(remote);
        }
        Ok(FriendStatus::Pending)
    }
    .await;
    let response = match result {
        Ok(status) => Response::ok(Some(
            match status {
                FriendStatus::Pending => "pending",
                FriendStatus::Accepted => "accepted",
                FriendStatus::Declined => "declined",
            }
            .into(),
        )),
        Err(e) => Response::error(e),
    };
    send.write_all(&serde_json::to_vec(&response)?).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    Ok(())
}

async fn accept_offer(
    conn: Connection,
    allowed: Arc<RwLock<HashSet<iroh::EndpointId>>>,
    tx: mpsc::Sender<(iroh::EndpointId, Offer)>,
) -> Result<()> {
    let remote = conn.remote_id();
    let (mut send, mut recv) = conn.accept_bi().await?;
    let bytes = recv.read_to_end(16000).await?;
    let result: Result<()> = async {
        ensure!(
            allowed.read().await.contains(&remote),
            "Add each other's Snapcodes before sending"
        );
        let offer: Offer = serde_json::from_slice(&bytes)?;
        ensure!(
            offer.caption.len() <= 2000 && ["snap", "story", "text"].contains(&offer.kind.as_str()),
            "Invalid snap"
        );
        ensure!(
            offer.ticket.expires_at > now() && offer.ticket.expires_at <= now() + 86460,
            "Invalid expiry"
        );
        ensure!(allowed_mime(&offer.ticket.mime), "Unsupported media");
        tx.send((remote, offer)).await?;
        Ok(())
    }
    .await;
    let response = match result {
        Ok(()) => Response::ok(None),
        Err(e) => Response::error(e),
    };
    send.write_all(&serde_json::to_vec(&response)?).await?;
    send.finish()?;
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    Ok(())
}

pub fn decrypt(ticket: &Ticket, data: &str) -> Result<Vec<u8>> {
    let key = hex::decode(&ticket.key)?;
    let nonce = hex::decode(&ticket.nonce)?;
    ensure!(nonce.len() == 12, "Invalid nonce");
    let cipher =
        Aes256Gcm::new_from_slice(&key).map_err(|_| anyhow::anyhow!("Invalid media key"))?;
    cipher
        .decrypt(Nonce::from_slice(&nonce), STANDARD.decode(data)?.as_ref())
        .map_err(|_| anyhow::anyhow!("Media authentication failed"))
}
pub fn allowed_mime(m: &str) -> bool {
    [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/gif",
        "video/mp4",
        "video/webm",
        "video/quicktime",
        "text/plain",
    ]
    .contains(&m)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn invitation(client: &Client, name: &str) -> Invite {
        Invite {
            version: 1,
            name: name.into(),
            handle: String::new(),
            did: String::new(),
            endpoint: client.endpoint.addr(),
            request_token: client.request_token.clone(),
        }
    }

    #[cfg(feature = "app")]
    #[derive(Default)]
    struct FixtureDirectory(std::sync::RwLock<HashSet<(String, iroh::EndpointId)>>);
    #[cfg(feature = "app")]
    impl crate::devices::DeviceDirectory for FixtureDirectory {
        fn check<'a>(
            &'a self,
            did: &'a str,
            endpoint: iroh::EndpointId,
        ) -> futures_util::future::BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                ensure!(
                    self.0.read().unwrap().contains(&(did.into(), endpoint)),
                    "Record revoked or wrong device"
                );
                Ok(())
            })
        }
        fn profile<'a>(
            &'a self,
            did: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<crate::discovery::Profile>> {
            Box::pin(async move {
                Ok(crate::discovery::Profile {
                    did: did.into(),
                    handle: "verified.test".into(),
                    name: "Verified account".into(),
                    description: String::new(),
                    avatar: None,
                })
            })
        }
    }
    #[cfg(feature = "app")]
    #[tokio::test]
    async fn account_requests_bind_remote_keys_require_consent_and_recheck_revocation() -> Result<()>
    {
        let dir = std::env::temp_dir().join(format!("n0-account-peers-{}", uuid::Uuid::new_v4()));
        let directory = Arc::new(FixtureDirectory::default());
        let (tx_a, _) = mpsc::channel(4);
        let (friends_a, _) = mpsc::channel(4);
        let (tx_b, mut media) = mpsc::channel(4);
        let (friends_b, mut friends) = mpsc::channel(4);
        let alice = Client::start_inner(dir.join("a"), tx_a, friends_a, directory.clone()).await?;
        let bob = Client::start_inner(dir.join("b"), tx_b, friends_b, directory.clone()).await?;
        *alice.accounts.own.write().await = Some("did:plc:alice".into());
        *bob.accounts.own.write().await = Some("did:plc:bob".into());
        directory.0.write().unwrap().extend([
            ("did:plc:alice".into(), alice.endpoint.id()),
            ("did:plc:bob".into(), bob.endpoint.id()),
        ]);
        // Seed a LAN route while retaining authenticated iroh key matching.
        let connection = alice
            .endpoint
            .connect(bob.endpoint.addr(), crate::devices::FRIEND_ALPN)
            .await?;
        connection.close(0u32.into(), b"route known");
        let mut target = invitation(&bob, "Bob");
        target.version = 2;
        target.did = "did:plc:bob".into();
        target.request_token.clear();
        let own = invitation(&alice, "Spoofed label");
        assert_eq!(
            alice.request_friend(&target, own.clone()).await?,
            FriendStatus::Pending
        );
        let received = friends.recv().await.context("Expected verified request")?;
        assert_eq!(received.version, 2);
        assert_eq!(received.did, "did:plc:alice");
        assert_eq!(received.name, "Verified account");
        assert!(received.request_token.is_empty());
        assert!(
            bob.allowed.read().await.is_empty(),
            "Registration alone must not grant access"
        );
        let mut wrong = target.clone();
        wrong.did = "did:plc:alice".into();
        assert!(alice.request_friend(&wrong, own.clone()).await.is_err());
        // A signed-in client cannot send a claimed issuer unrelated to its TLS key.
        let connection = alice
            .endpoint
            .connect(bob.endpoint.addr(), crate::devices::FRIEND_ALPN)
            .await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        let mut impersonation = received.clone();
        impersonation.did = "did:plc:bob".into();
        send.write_all(&serde_json::to_vec(&FriendHello {
            token: String::new(),
            invite: impersonation,
        })?)
        .await?;
        send.finish()?;
        let response: Response = serde_json::from_slice(&recv.read_to_end(4096).await?)?;
        assert!(!response.ok);
        connection.close(0u32.into(), b"done");
        bob.authorize_friend(&received).await?;
        assert_eq!(
            alice.request_friend(&target, own.clone()).await?,
            FriendStatus::Accepted
        );
        alice.authorize_friend(&target).await?;
        let ticket = alice
            .upload(
                b"private",
                "text/plain",
                alice.local_store.endpoint.addr(),
                alice.local_store.token.clone(),
            )
            .await?;
        let offer = Offer {
            id: uuid::Uuid::new_v4().to_string(),
            caption: String::new(),
            kind: "text".into(),
            ticket,
            created_at: now(),
        };
        alice.send(bob.endpoint.addr(), &offer).await?;
        assert_eq!(media.recv().await.context("Expected media")?.1.id, offer.id);
        directory
            .0
            .write()
            .unwrap()
            .remove(&("did:plc:alice".into(), alice.endpoint.id()));
        assert!(alice.request_friend(&target, own).await.is_err());
        assert!(alice.send(bob.endpoint.addr(), &offer).await.is_err());
        assert!(
            bob.accounts
                .verify_media(bob.endpoint.id(), alice.endpoint.id())
                .await
                .is_err()
        );
        assert!(bob.authorize_friend(&received).await.is_err());
        alice.endpoint.close().await;
        bob.endpoint.close().await;
        alice.local_store.endpoint.close().await;
        bob.local_store.endpoint.close().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn friend_requests_require_code_and_acceptance_before_media() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("flicker-friends-{}", uuid::Uuid::new_v4()));
        let (offers_a, _) = mpsc::channel(4);
        let (offers_b, mut media) = mpsc::channel(4);
        let (friends_a, _) = mpsc::channel(4);
        let (friends_b, mut requests) = mpsc::channel(4);
        let alice = Client::start_with_requests(dir.join("a"), offers_a, friends_a).await?;
        let bob = Client::start_with_requests(dir.join("b"), offers_b, friends_b).await?;
        let target = invitation(&bob, "Bob");
        let own = invitation(&alice, "Alice");
        let mut bad = target.clone();
        bad.request_token = random_hex();
        assert!(alice.request_friend(&bad, own.clone()).await.is_err());
        // A peer cannot introduce someone else's endpoint as their own.
        assert!(alice.request_friend(&target, target.clone()).await.is_err());
        assert!(requests.try_recv().is_err());
        assert_eq!(
            alice.request_friend(&target, own.clone()).await?,
            FriendStatus::Pending
        );
        let received = tokio::time::timeout(Duration::from_secs(2), requests.recv())
            .await?
            .context("Expected request")?;
        assert_eq!(received.endpoint.id, alice.endpoint.id());
        assert_eq!(
            alice.request_friend(&target, own.clone()).await?,
            FriendStatus::Pending
        );
        assert!(
            requests.try_recv().is_err(),
            "Retries must not duplicate requests"
        );
        let ticket = alice
            .upload(
                b"private",
                "text/plain",
                alice.local_store.endpoint.addr(),
                alice.local_store.token.clone(),
            )
            .await?;
        let offer = Offer {
            id: uuid::Uuid::new_v4().to_string(),
            caption: String::new(),
            kind: "text".into(),
            ticket,
            created_at: now(),
        };
        assert!(
            alice.send(bob.endpoint.addr(), &offer).await.is_err(),
            "Requesting cannot authorize media"
        );
        bob.declined.write().await.insert(alice.endpoint.id());
        assert_eq!(
            alice.request_friend(&target, own.clone()).await?,
            FriendStatus::Declined
        );
        assert!(alice.send(bob.endpoint.addr(), &offer).await.is_err());
        bob.declined.write().await.remove(&alice.endpoint.id());
        bob.allowed.write().await.insert(alice.endpoint.id());
        assert_eq!(
            alice.request_friend(&target, own).await?,
            FriendStatus::Accepted
        );
        alice.send(bob.endpoint.addr(), &offer).await?;
        let (_, received) = tokio::time::timeout(Duration::from_secs(2), media.recv())
            .await?
            .context("Expected media")?;
        assert_eq!(received.id, offer.id);
        alice.endpoint.close().await;
        bob.endpoint.close().await;
        alice.local_store.endpoint.close().await;
        bob.local_store.endpoint.close().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn encrypted_store_roundtrip_and_bad_capability() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("flicker-test-{}", uuid::Uuid::new_v4()));
        let (tx, _) = mpsc::channel(2);
        let client = Client::start(dir.clone(), tx).await?;
        let addr = client.local_store.endpoint.addr();
        let ticket = client
            .upload(
                b"private photo",
                "image/jpeg",
                addr.clone(),
                client.local_store.token.clone(),
            )
            .await?;
        let data = client.download_cipher(&ticket).await?;
        assert_eq!(decrypt(&ticket, &data)?, b"private photo");
        assert!(!data.contains("private photo"));
        let mut bad = ticket.clone();
        bad.read_cap = random_hex();
        assert!(client.download_cipher(&bad).await.is_err());
        assert!(
            client
                .probe(addr.clone(), "wrong token".into())
                .await
                .is_err()
        );
        // Expiry is checked by the host, independently of the UI's countdown.
        let path = dir.join("host/objects").join(format!("{}.json", ticket.id));
        let mut stored: Stored = serde_json::from_slice(&std::fs::read(&path)?)?;
        stored.expires_at = now().saturating_sub(1);
        std::fs::write(&path, serde_json::to_vec(&stored)?)?;
        assert!(
            request(
                &client.endpoint,
                addr,
                Request::Get {
                    id: ticket.id.clone(),
                    read_cap: ticket.read_cap.clone(),
                }
            )
            .await
            .is_err()
        );
        let mut bad = ticket;
        bad.key = random_hex();
        assert!(decrypt(&bad, &data).is_err());
        client.endpoint.close().await;
        client.local_store.endpoint.close().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[tokio::test]
    async fn peers_require_explicit_approval_and_can_fetch_from_sender_host() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("flicker-peers-{}", uuid::Uuid::new_v4()));
        let (tx_a, _) = mpsc::channel(2);
        let (tx_b, mut inbox) = mpsc::channel(2);
        let alice = Client::start(dir.join("alice"), tx_a).await?;
        let bob = Client::start(dir.join("bob"), tx_b).await?;
        let ticket = alice
            .upload(
                b"hello bob",
                "text/plain",
                alice.local_store.endpoint.addr(),
                alice.local_store.token.clone(),
            )
            .await?;
        let offer = Offer {
            id: uuid::Uuid::new_v4().to_string(),
            caption: "A message".into(),
            kind: "text".into(),
            ticket,
            created_at: now(),
        };
        assert!(alice.send(bob.endpoint.addr(), &offer).await.is_err());
        bob.allowed.write().await.insert(alice.endpoint.id());
        alice.send(bob.endpoint.addr(), &offer).await?;
        let (sender, received) = tokio::time::timeout(Duration::from_secs(2), inbox.recv())
            .await?
            .context("Expected a delivered invitation")?;
        assert_eq!(sender, alice.endpoint.id());
        assert_eq!(received.id, offer.id);
        // The sender's inbox can close after delivery; its media host is separate.
        alice.endpoint.close().await;
        let cipher = bob.download_cipher(&received.ticket).await?;
        assert_eq!(decrypt(&received.ticket, &cipher)?, b"hello bob");
        bob.endpoint.close().await;
        alice.local_store.endpoint.close().await;
        bob.local_store.endpoint.close().await;
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
