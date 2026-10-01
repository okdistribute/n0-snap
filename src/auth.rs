//! Native AT Protocol OAuth. Passwords are entered only in the system browser.
use crate::{
    discovery::{Discovery, Profile},
    model::private_write,
};
use anyhow::{Context, Result, bail, ensure};
use atrium_api::{agent::SessionManager, types::string::Did};
use atrium_common::store::Store;
use atrium_identity::{
    did::{CommonDidResolver, CommonDidResolverConfig, DEFAULT_PLC_DIRECTORY_URL},
    handle::{AtprotoHandleResolver, AtprotoHandleResolverConfig, DnsTxtResolver},
};
use atrium_oauth::{
    AtprotoLocalhostClientMetadata, AuthorizeOptions, CallbackParams, KnownScope, OAuthClient,
    OAuthClientConfig, OAuthResolverConfig, OAuthSession, Scope,
    store::{
        session::{Session, SessionStore},
        state::MemoryStateStore,
    },
};
use atrium_xrpc::{
    HttpClient,
    http::{Request, Response},
};
use futures_util::FutureExt;
use hickory_resolver::TokioAsyncResolver;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Mutex,
};

const REDIRECT: &str = "http://127.0.0.1:47839/oauth/callback";
const APPVIEW: &str = "did:web:api.bsky.app";
const METHODS: [&str; 3] = [
    "app.bsky.actor.getProfile",
    "app.bsky.actor.searchActors",
    "app.bsky.graph.getFollows",
];

fn scopes() -> Vec<Scope> {
    std::iter::once(Scope::Known(KnownScope::Atproto))
        .chain(METHODS.map(|m| Scope::Unknown(format!("rpc:{m}?aud={APPVIEW}#bsky_appview"))))
        .chain([Scope::Unknown(format!(
            "repo:{}",
            crate::devices::COLLECTION
        ))])
        .collect()
}

#[cfg(test)]
type MockHttp = Arc<dyn Fn(Request<Vec<u8>>) -> Response<Vec<u8>> + Send + Sync>;
#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    #[cfg(test)]
    mock: Option<MockHttp>,
}
impl Http {
    fn new() -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(20))
                .build()?,
            #[cfg(test)]
            mock: None,
        })
    }
}
impl HttpClient for Http {
    async fn send_http(
        &self,
        request: Request<Vec<u8>>,
    ) -> std::result::Result<Response<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(test)]
        if let Some(mock) = &self.mock {
            return Ok(mock(request));
        }
        let response = self.client.execute(request.try_into()?).await?;
        let mut builder = Response::builder().status(response.status());
        for (key, value) in response.headers() {
            builder = builder.header(key, value);
        }
        Ok(builder.body(response.bytes().await?.to_vec())?)
    }
}

pub struct Dns(TokioAsyncResolver);
impl DnsTxtResolver for Dns {
    async fn resolve(
        &self,
        query: &str,
    ) -> std::result::Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self
            .0
            .txt_lookup(query)
            .await?
            .iter()
            .map(|txt| txt.to_string())
            .collect())
    }
}

/// Contains credentials; deliberately does not implement Debug.
#[derive(Clone)]
pub struct Sessions {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}
impl Sessions {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("auth/session.json"),
            lock: Arc::new(Mutex::new(())),
        }
    }
    fn read(&self) -> io::Result<Option<Session>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn write(&self, session: Option<&Session>) -> io::Result<()> {
        let temp = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec(&session).map_err(io::Error::other)?;
        private_write(&temp, &bytes).map_err(io::Error::other)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::set_permissions(
                self.path.parent().unwrap(),
                std::fs::Permissions::from_mode(0o700),
            )?;
        }
        std::fs::rename(temp, &self.path)
    }
}
impl Store<Did, Session> for Sessions {
    type Error = io::Error;
    async fn get(&self, key: &Did) -> io::Result<Option<Session>> {
        let _guard = self.lock.lock().await;
        Ok(self.read()?.filter(|s| &s.token_set.sub == key))
    }
    async fn set(&self, key: Did, value: Session) -> io::Result<()> {
        if key != value.token_set.sub {
            return Err(io::Error::other("Session identity mismatch"));
        }
        let _guard = self.lock.lock().await;
        self.write(Some(&value))
    }
    async fn del(&self, key: &Did) -> io::Result<()> {
        let _guard = self.lock.lock().await;
        if self.read()?.is_some_and(|s| &s.token_set.sub == key) {
            self.write(None)?;
        }
        Ok(())
    }
    async fn clear(&self) -> io::Result<()> {
        let _guard = self.lock.lock().await;
        self.write(None)
    }
}
impl SessionStore for Sessions {}

type DidResolver = CommonDidResolver<Http>;
type HandleResolver = AtprotoHandleResolver<Dns, Http>;
type Client = OAuthClient<MemoryStateStore, Sessions, DidResolver, HandleResolver, Http>;
pub type BlueskySession = OAuthSession<Http, DidResolver, HandleResolver, Sessions>;

#[derive(Clone)]
pub struct Account {
    pub profile: Profile,
    pub session: Arc<BlueskySession>,
}
impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.profile == other.profile && Arc::ptr_eq(&self.session, &other.session)
    }
}

fn client(sessions: Sessions, states: MemoryStateStore) -> Result<Client> {
    client_with_http(sessions, states, Http::new()?)
}
fn client_with_http(sessions: Sessions, states: MemoryStateStore, http: Http) -> Result<Client> {
    let shared = Arc::new(http.clone());
    Ok(OAuthClient::new(OAuthClientConfig {
        client_metadata: AtprotoLocalhostClientMetadata {
            redirect_uris: Some(vec![REDIRECT.into()]),
            scopes: Some(scopes()),
        },
        keys: None,
        resolver: OAuthResolverConfig {
            did_resolver: CommonDidResolver::new(CommonDidResolverConfig {
                plc_directory_url: DEFAULT_PLC_DIRECTORY_URL.into(),
                http_client: shared.clone(),
            }),
            handle_resolver: AtprotoHandleResolver::new(AtprotoHandleResolverConfig {
                dns_txt_resolver: Dns(dns_resolver()?),
                http_client: shared,
            }),
            authorization_server_metadata: Default::default(),
            protected_resource_metadata: Default::default(),
        },
        state_store: states,
        session_store: sessions,
        http_client: http,
    })?)
}

fn dns_resolver() -> Result<TokioAsyncResolver> {
    // iOS has no readable /etc/resolv.conf. Match iroh's fallback resolver.
    #[cfg(target_os = "ios")]
    return Ok(TokioAsyncResolver::tokio(
        hickory_resolver::config::ResolverConfig::google(),
        hickory_resolver::config::ResolverOpts::default(),
    ));
    #[cfg(not(target_os = "ios"))]
    Ok(TokioAsyncResolver::tokio_from_system_conf()?)
}

async fn account(session: BlueskySession) -> Result<Account> {
    let did = session
        .did()
        .await
        .context("Bluesky did not return an account identity")?;
    let session = Arc::new(session);
    // An authenticated request validates restored tokens and triggers refresh if needed.
    let profile = Discovery::authenticated(session.clone())?
        .profile(did.as_str())
        .await?;
    ensure!(
        profile.did == did.as_str(),
        "Bluesky returned a different account"
    );
    Ok(Account { profile, session })
}

pub async fn restore(root: &Path) -> Result<Option<Account>> {
    let sessions = Sessions::new(root);
    let Some(saved) = sessions
        .read()
        .context("Could not read your saved sign-in")?
    else {
        return Ok(None);
    };
    let client = client(sessions, MemoryStateStore::default())?;
    let session = client.restore(&saved.token_set.sub).await.context(
        "Could not restore your Bluesky session. Check your connection or sign in again.",
    )?;
    account(session).await.map(Some)
}

pub async fn logout(root: &Path) -> Result<()> {
    let sessions = Sessions::new(root);
    // Forget locally even when the server cannot be reached. Keep snaps and memories.
    let saved = sessions.read()?;
    let revoke = if let Some(saved) = saved {
        let client = client(sessions.clone(), MemoryStateStore::default());
        match client {
            Ok(client) => {
                tokio::time::timeout(Duration::from_secs(5), client.revoke(&saved.token_set.sub))
                    .await
                    .is_ok_and(|r| r.is_ok())
            }
            Err(_) => false,
        }
    } else {
        true
    };
    sessions
        .clear()
        .await
        .context("Could not remove saved sign-in")?;
    ensure!(
        revoke,
        "Signed out on this device. Bluesky could not be reached to revoke the session; you can revoke it in Bluesky settings."
    );
    Ok(())
}

pub struct PendingLogin {
    pub url: String,
    listener: TcpListener,
    client: Client,
    states: MemoryStateStore,
}
pub async fn begin(root: &Path, handle: &str) -> Result<PendingLogin> {
    let handle = handle.trim().trim_start_matches('@').to_lowercase();
    let _: atrium_api::types::string::Handle = handle.parse().map_err(|_| {
        anyhow::anyhow!("Enter your full Bluesky handle, such as alice.bsky.social")
    })?;
    let listener = TcpListener::bind("127.0.0.1:47839").await.context(
        "Could not open the sign-in callback. Close other n0-snap sign-in windows and try again.",
    )?;
    let states = MemoryStateStore::default();
    let client = client(Sessions::new(root), states.clone())?;
    let url = std::panic::AssertUnwindSafe(client.authorize(
        &handle,
        AuthorizeOptions {
            scopes: scopes(),
            ..Default::default()
        },
    ))
    .catch_unwind()
    .await
    .map_err(|_| anyhow::anyhow!("This account's server could not start sign-in"))?
    .context("Could not start Bluesky sign-in. Check your handle and connection.")?;
    Ok(PendingLogin {
        url,
        listener,
        client,
        states,
    })
}

impl PendingLogin {
    pub async fn finish(self) -> Result<Account> {
        tokio::time::timeout(Duration::from_secs(300), self.wait())
            .await
            .context("Sign-in timed out. Please try again.")?
    }
    async fn wait(self) -> Result<Account> {
        loop {
            let (mut stream, _) = self.listener.accept().await?;
            let request =
                match tokio::time::timeout(Duration::from_secs(3), read_request(&mut stream)).await
                {
                    Ok(Ok(request)) => request,
                    _ => {
                        let _ = reply(&mut stream, "400 Bad Request", "Invalid request.").await;
                        continue;
                    }
                };
            let params = match callback_query(&request) {
                Ok(params) => params,
                Err(_) => {
                    let _ = reply(&mut stream, "400 Bad Request", "Invalid callback.").await;
                    continue;
                }
            };
            let Some(state_key) = params.get("state") else {
                continue;
            };
            let Some(state) = self.states.get(state_key).await? else {
                let _ = reply(
                    &mut stream,
                    "400 Bad Request",
                    "This sign-in is no longer active.",
                )
                .await;
                continue;
            };
            if params.get("iss") != Some(&state.iss) {
                let _ = reply(
                    &mut stream,
                    "400 Bad Request",
                    "Sign-in server did not match.",
                )
                .await;
                continue;
            }
            if params.contains_key("error") {
                let _ = reply(
                    &mut stream,
                    "200 OK",
                    "Sign-in canceled. Return to n0-snap to try again.",
                )
                .await;
                bail!("Bluesky sign-in was canceled. Please try again.");
            }
            let Some(code) = params.get("code").filter(|code| !code.is_empty()) else {
                continue;
            };
            let result = std::panic::AssertUnwindSafe(self.client.callback(CallbackParams {
                code: code.clone(),
                state: Some(state_key.clone()),
                iss: Some(state.iss),
            }))
            .catch_unwind()
            .await;
            let session = match result {
                Ok(Ok((session, _))) => session,
                _ => {
                    let _ = reply(
                        &mut stream,
                        "400 Bad Request",
                        "Sign-in failed. Return to n0-snap and try again.",
                    )
                    .await;
                    bail!("Bluesky could not complete sign-in. Please try again.");
                }
            };
            let result = account(session).await;
            let body = if result.is_ok() {
                "Signed in. You can close this tab and return to n0-snap."
            } else {
                "Could not load your account. Return to n0-snap and try again."
            };
            let _ = reply(&mut stream, "200 OK", body).await;
            return result;
        }
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 1024];
    while !bytes.windows(4).any(|b| b == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await?;
        ensure!(
            read > 0 && bytes.len() + read <= 16384,
            "Invalid callback request"
        );
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(String::from_utf8(bytes)?)
}
fn callback_query(request: &str) -> Result<std::collections::HashMap<String, String>> {
    let mut lines = request.lines();
    let parts: Vec<_> = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    ensure!(
        parts.len() == 3 && parts[0] == "GET" && parts[2] == "HTTP/1.1",
        "Invalid callback method"
    );
    let (path, query) = parts[1].split_once('?').context("Missing callback query")?;
    ensure!(path == "/oauth/callback", "Invalid callback path");
    let host = lines.find_map(|l| {
        l.split_once(':')
            .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.trim())
    });
    ensure!(host == Some("127.0.0.1:47839"), "Invalid callback host");
    let pairs: Vec<(String, String)> = reqwest::Url::parse(&format!("{REDIRECT}?{query}"))?
        .query_pairs()
        .into_owned()
        .collect();
    let mut params = std::collections::HashMap::new();
    for (key, value) in pairs {
        ensure!(
            params.insert(key, value).is_none(),
            "Duplicate callback parameter"
        );
    }
    Ok(params)
}
async fn reply(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mock_provider(
        par: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
    ) -> Http {
        Http {
            client: reqwest::Client::new(),
            mock: Some(Arc::new(move |request| {
                let path = request.uri().path();
                let mut status = 200;
                let value = match path {
                    "/did:plc:alice" => {
                        json!({"id":"did:plc:alice", "service":[{"id":"#atproto_pds", "type":"AtprotoPersonalDataServer", "serviceEndpoint":"https://pds.example.test"}]})
                    }
                    "/.well-known/oauth-protected-resource" => {
                        json!({"resource":"https://pds.example.test", "authorization_servers":["https://auth.example.test"], "scopes_supported":[]})
                    }
                    "/.well-known/oauth-authorization-server" => json!({
                        "issuer":"https://auth.example.test", "authorization_endpoint":"https://auth.example.test/authorize",
                        "token_endpoint":"https://auth.example.test/token", "revocation_endpoint":"https://auth.example.test/revoke",
                        "pushed_authorization_request_endpoint":"https://auth.example.test/par",
                        "scopes_supported":["atproto"], "response_types_supported":["code"],
                        "token_endpoint_auth_methods_supported":["none"], "dpop_signing_alg_values_supported":["ES256"],
                        "authorization_response_iss_parameter_supported":true
                    }),
                    "/par" => {
                        assert!(request.headers().contains_key("DPoP"));
                        let params: std::collections::HashMap<String, String> =
                            serde_html_form::from_bytes(request.body()).unwrap();
                        assert_eq!(params["code_challenge_method"], "S256");
                        assert_eq!(params["redirect_uri"], REDIRECT);
                        assert!(params["state"].len() >= 16);
                        *par.lock().unwrap() = params;
                        status = 201;
                        json!({"request_uri":"urn:ietf:params:oauth:request_uri:test", "expires_in":300})
                    }
                    "/token" => {
                        use base64::Engine;
                        use sha2::{Digest, Sha256};
                        assert!(request.headers().contains_key("DPoP"));
                        let params: std::collections::HashMap<String, String> =
                            serde_html_form::from_bytes(request.body()).unwrap();
                        if params["grant_type"] == "authorization_code" {
                            assert_eq!(params["code"], "test-code");
                            assert_eq!(
                                base64::engine::general_purpose::URL_SAFE_NO_PAD
                                    .encode(Sha256::digest(params["code_verifier"].as_bytes())),
                                par.lock().unwrap()["code_challenge"]
                            );
                        } else {
                            assert_eq!(params["refresh_token"], "test-refresh");
                        }
                        json!({"access_token":"test-access", "refresh_token":"test-refresh", "token_type":"DPoP", "expires_in":3600, "scope":"atproto", "sub":"did:plc:alice"})
                    }
                    "/xrpc/app.bsky.actor.getProfile" => {
                        assert!(request.headers().contains_key("DPoP"));
                        assert_eq!(
                            request.headers()["atproto-proxy"],
                            "did:web:api.bsky.app#bsky_appview"
                        );
                        if request.headers()["authorization"] == "DPoP expired" {
                            return Response::builder()
                                .status(401)
                                .header("WWW-Authenticate", "DPoP error=\"invalid_token\"")
                                .body(Vec::new())
                                .unwrap();
                        }
                        assert_eq!(request.headers()["authorization"], "DPoP test-access");
                        json!({"did":"did:plc:alice", "handle":"alice.test", "displayName":"Alice"})
                    }
                    "/xrpc/com.atproto.repo.putRecord" | "/xrpc/com.atproto.repo.deleteRecord" => {
                        assert!(
                            !request.headers().contains_key("atproto-proxy"),
                            "Repository writes must never go to AppView"
                        );
                        assert!(request.headers().contains_key("DPoP"));
                        assert_eq!(request.headers()["authorization"], "DPoP test-access");
                        let body: serde_json::Value =
                            serde_json::from_slice(request.body()).unwrap();
                        assert_eq!(body["repo"], "did:plc:alice");
                        assert_eq!(body["collection"], crate::devices::COLLECTION);
                        assert_eq!(body["rkey"].as_str().unwrap().len(), 64);
                        if path.ends_with("putRecord") {
                            assert_eq!(body["validate"], false);
                            assert_eq!(body["record"]["iss"], "did:plc:alice");
                            assert_eq!(body["record"].as_object().unwrap().len(), 5);
                            assert!(body["record"]["proof"]["$bytes"].is_string());
                            assert!(body["record"].get("request_token").is_none());
                        }
                        json!({})
                    }
                    "/revoke" => return Response::builder().status(204).body(Vec::new()).unwrap(),
                    _ => panic!("Unexpected mock provider request: {path}"),
                };
                Response::builder()
                    .status(status)
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(&value).unwrap())
                    .unwrap()
            })),
        }
    }

    async fn send_callback(port: u16, query: &str) -> Result<String> {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).await?;
        socket
            .write_all(
                format!("GET /oauth/callback?{query} HTTP/1.1\r\nHost: 127.0.0.1:47839\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut response = String::new();
        socket.read_to_string(&mut response).await?;
        Ok(response)
    }

    #[tokio::test]
    async fn browser_callback_validates_state_then_restores_refreshes_and_revokes() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("n0-snap-oauth-flow-{}", uuid::Uuid::new_v4()));
        let sessions = Sessions::new(&root);
        let states = MemoryStateStore::default();
        let par = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let http = mock_provider(par.clone());
        let client = client_with_http(sessions.clone(), states.clone(), http.clone())?;
        let url = client
            .authorize(
                "did:plc:alice",
                AuthorizeOptions {
                    scopes: scopes(),
                    ..Default::default()
                },
            )
            .await?;
        let state = par.lock().unwrap()["state"].clone();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let pending = PendingLogin {
            url,
            listener,
            client,
            states: states.clone(),
        };
        let finish = tokio::spawn(pending.finish());
        assert!(
            send_callback(
                port,
                "state=wrong&code=test-code&iss=https%3A%2F%2Fauth.example.test"
            )
            .await?
            .contains("400 Bad Request")
        );
        assert!(
            send_callback(
                port,
                &format!("state={state}&code=test-code&iss=https%3A%2F%2Fattacker.test")
            )
            .await?
            .contains("400 Bad Request")
        );
        assert!(sessions.read()?.is_none());
        assert!(
            send_callback(
                port,
                &format!("state={state}&code=test-code&iss=https%3A%2F%2Fauth.example.test")
            )
            .await?
            .contains("Signed in.")
        );
        assert_eq!(finish.await??.profile.did, "did:plc:alice");
        assert!(
            states.get(&state).await?.is_none(),
            "Callback cannot be replayed"
        );
        let mut saved = sessions.read()?.unwrap();
        let did = saved.token_set.sub.clone();
        saved.token_set.access_token = "expired".into();
        saved.token_set.expires_at = Some("2000-01-01T00:00:00Z".parse().unwrap());
        sessions.set(did.clone(), saved).await?;
        let restored_client =
            client_with_http(Sessions::new(&root), MemoryStateStore::default(), http)?;
        let restored = account(restored_client.restore(&did).await?).await?;
        assert_eq!(restored.profile.handle, "alice.test");
        let device_key = iroh::SecretKey::generate();
        crate::devices::write_record(&restored, &device_key).await?;
        crate::devices::revoke(&restored, device_key.public()).await?;
        assert_eq!(
            sessions.read()?.unwrap().token_set.access_token,
            "test-access"
        );
        restored_client.revoke(&did).await?;
        assert!(sessions.read()?.is_none());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[tokio::test]
    #[ignore = "contacts Bluesky's live authorization server; does not sign in"]
    async fn live_bluesky_accepts_authorization_request() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("n0-snap-auth-probe-{}", uuid::Uuid::new_v4()));
        let pending = begin(&root, "bsky.app").await?;
        let url = reqwest::Url::parse(&pending.url)?;
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("bsky.social"));
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "request_uri" && !value.is_empty())
        );
        assert!(
            !root.exists(),
            "Starting sign-in must not persist credentials"
        );
        Ok(())
    }

    #[tokio::test]
    async fn session_storage_is_private_scoped_and_cleared_on_logout() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("n0-snap-auth-store-{}", uuid::Uuid::new_v4()));
        let sessions = Sessions::new(&root);
        let session: Session = serde_json::from_value(serde_json::json!({
            "dpop_key": {"kty":"EC", "crv":"P-256", "x":"NIRNgPVAwnVNzN5g2Ik2IMghWcjnBOGo9B-lKXSSXFs", "y":"iWF-Of43XoSTZxcadO9KWdPTjiCoviSztYw7aMtZZMc", "d":"9MuCYfKK4hf95p_VRj6cxKJwORTgvEU3vynfmSgFH2M"},
            "token_set": {"iss":"https://example.test", "sub":"did:plc:alice", "aud":"https://pds.example.test", "scope":"atproto", "refresh_token":"test-refresh", "access_token":"test-access", "token_type":"DPoP", "expires_at":null}
        }))?;
        let alice = session.token_set.sub.clone();
        let bob: Did = "did:plc:bob".parse().unwrap();
        assert!(sessions.read()?.is_none());
        assert!(sessions.set(bob.clone(), session.clone()).await.is_err());
        sessions.set(alice.clone(), session.clone()).await?;
        assert_eq!(Sessions::new(&root).get(&alice).await?, Some(session));
        assert!(sessions.get(&bob).await?.is_none());
        sessions.del(&bob).await?;
        assert!(sessions.get(&alice).await?.is_some());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&sessions.path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        sessions.clear().await?;
        assert!(Sessions::new(&root).read()?.is_none());
        assert!(!std::fs::read_to_string(&sessions.path)?.contains("test-refresh"));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn callback_rejects_wrong_origin_path_method_and_duplicate_state() {
        let valid = "GET /oauth/callback?code=abc&state=random&iss=https%3A%2F%2Fbsky.social HTTP/1.1\r\nHost: 127.0.0.1:47839\r\n\r\n";
        assert_eq!(callback_query(valid).unwrap()["iss"], "https://bsky.social");
        for invalid in [
            valid.replace("GET ", "POST "),
            valid.replace("/oauth/callback?", "/other?"),
            valid.replace("Host: 127.0.0.1:47839", "Host: attacker.test"),
            valid.replace("state=random", "state=random&state=other"),
        ] {
            assert!(callback_query(&invalid).is_err());
        }
    }
    #[test]
    fn permissions_only_allow_discovery_and_our_device_collection() {
        let scopes = scopes();
        assert_eq!(scopes.len(), 5);
        assert_eq!(scopes[0].as_ref(), "atproto");
        assert!(
            scopes
                .iter()
                .skip(1)
                .take(3)
                .all(|s| s.as_ref().starts_with("rpc:") && !s.as_ref().contains("chat"))
        );
        assert_eq!(
            scopes[4].as_ref(),
            format!("repo:{}", crate::devices::COLLECTION)
        );
    }
}
