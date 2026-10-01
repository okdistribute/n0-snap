//! Experimental atproto + iroh device records. Records are public, never secrets.
pub mod proof;
use crate::{auth::Account, model::Invite};
use anyhow::{Context, Result, bail, ensure};
use atrium_xrpc::{XrpcClient, XrpcRequest, http::Method};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use ipld_core::ipld::Ipld;
use iroh::{EndpointId, SecretKey};
use serde_json::{Value, json};
use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Duration};

pub const COLLECTION: &str = "io.github.okdistribute.n0snap.device";
pub const FRIEND_ALPN: &[u8] = b"io.github.okdistribute.n0snap.friend/1";
pub const VIA: &str = "io.github.okdistribute.n0snap.friend/1";

/// Narrow boundary shared by the native connection handlers and test fixtures.
pub trait DeviceDirectory: Send + Sync {
    fn check<'a>(
        &'a self,
        did: &'a str,
        endpoint: EndpointId,
    ) -> futures_util::future::BoxFuture<'a, Result<()>>;
    fn profile<'a>(
        &'a self,
        did: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<crate::discovery::Profile>>;
}
impl DeviceDirectory for Directory {
    fn check<'a>(
        &'a self,
        did: &'a str,
        endpoint: EndpointId,
    ) -> futures_util::future::BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.verify(did, endpoint).await.map(|_| ()) })
    }
    fn profile<'a>(
        &'a self,
        did: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Result<crate::discovery::Profile>> {
        Box::pin(async move { crate::discovery::Discovery::new()?.profile(did).await })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedDevice {
    did: String,
    endpoint: EndpointId,
}
impl VerifiedDevice {
    pub fn endpoint(&self) -> EndpointId {
        self.endpoint
    }
    pub fn did(&self) -> &str {
        &self.did
    }
    pub fn invite(&self, name: String, handle: String) -> Invite {
        Invite {
            version: 2,
            name,
            handle,
            did: self.did.clone(),
            endpoint: self.endpoint.into(),
            request_token: String::new(),
        }
    }
}
fn unsigned(did: &str, endpoint: EndpointId) -> Ipld {
    let key = [&[0xed, 0x01][..], endpoint.as_bytes()].concat();
    Ipld::Map(BTreeMap::from([
        ("$type".into(), Ipld::String(COLLECTION.into())),
        ("iss".into(), Ipld::String(did.into())),
        (
            "sub".into(),
            Ipld::String(format!("did:key:z{}", bs58::encode(key).into_string())),
        ),
        ("via".into(), Ipld::List(vec![Ipld::String(VIA.into())])),
    ]))
}
pub fn record(did: &str, key: &SecretKey) -> Result<Value> {
    let unsigned = unsigned(did, key.public());
    let sig = key.sign(&proof::encode(&unsigned)?);
    let values = proof::map(&unsigned)?;
    Ok(json!({"$type":COLLECTION, "iss":did,
        "sub":proof::string(&values["sub"])?, "via":[VIA],
        "proof":{"$bytes":STANDARD_NO_PAD.encode(sig.to_bytes())}}))
}
fn verify_record(record: &Ipld, did: &str, endpoint: EndpointId) -> Result<VerifiedDevice> {
    let mut fields = proof::map(record)?.clone();
    let Ipld::Bytes(sig) = fields.remove("proof").context("Missing device signature")? else {
        bail!("Invalid device signature")
    };
    let signed = Ipld::Map(fields);
    ensure!(
        signed == unsigned(did, endpoint),
        "Device record identity or protocol mismatch"
    );
    let signature = iroh::Signature::from_bytes(
        &sig.try_into()
            .map_err(|_| anyhow::anyhow!("Invalid signature length"))?,
    );
    endpoint.verify(&proof::encode(&signed)?, &signature)?;
    Ok(VerifiedDevice {
        did: did.into(),
        endpoint,
    })
}

// Public directory lookups must not become an SSRF route into the user's LAN.
struct PublicDns;
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            !ip.is_private()
                && !ip.is_loopback()
                && !ip.is_link_local()
                && !ip.is_documentation()
                && a != 0
                && a < 224
                && !(a == 100 && (64..128).contains(&b))
                && !(a == 198 && (b == 18 || b == 19))
                && !(a == 192 && b == 0)
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
        }
    }
}
impl reqwest::dns::Resolve for PublicDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let name = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((name.as_str(), 443))
                .await?
                .collect();
            if addrs.is_empty() || addrs.iter().any(|a| !public_ip(a.ip())) {
                return Err(std::io::Error::other("Directory address is not public").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}
fn public_url(value: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value)?;
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url.port_or_known_default() == Some(443)
            && url.query().is_none()
            && url.fragment().is_none(),
        "Unsafe directory URL"
    );
    let host = url.host_str().context("Missing directory host")?;
    ensure!(
        host.parse::<IpAddr>().is_err()
            && !host.contains(':')
            && host.contains('.')
            && !host.ends_with('.'),
        "Directory must use a public DNS hostname"
    );
    Ok(url)
}
#[derive(Clone)]
pub struct Directory {
    http: reqwest::Client,
}
struct Identity {
    pds: reqwest::Url,
    key: String,
}
impl Directory {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(Arc::new(PublicDns))
                .timeout(Duration::from_secs(12))
                .build()?,
        })
    }
    async fn get(
        &self,
        url: reqwest::Url,
        params: &[(&str, &str)],
        limit: usize,
    ) -> Result<Vec<u8>> {
        let mut response = self
            .http
            .get(url)
            .header("Cache-Control", "no-cache")
            .query(params)
            .send()
            .await?
            .error_for_status()?;
        ensure!(
            response
                .content_length()
                .is_none_or(|len| len <= limit as u64),
            "Directory response is too large"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= limit,
                "Directory response is too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    async fn json(&self, url: reqwest::Url, params: &[(&str, &str)]) -> Result<Value> {
        Ok(serde_json::from_slice(
            &self.get(url, params, 256 * 1024).await?,
        )?)
    }
    async fn identity(&self, did: &str) -> Result<Identity> {
        let url = if let Some(id) = did.strip_prefix("did:plc:") {
            ensure!(
                id.len() == 24
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)),
                "Invalid PLC DID"
            );
            public_url(&format!("https://plc.directory/{did}"))?
        } else if let Some(domain) = did.strip_prefix("did:web:") {
            ensure!(
                !domain.contains([':', '/', '%', '?', '#', '@']),
                "Only domain-root did:web identities are supported"
            );
            public_url(&format!("https://{domain}/.well-known/did.json"))?
        } else {
            bail!("Unsupported account DID")
        };
        let doc = self.json(url, &[]).await?;
        ensure!(
            doc["id"].as_str() == Some(did),
            "DID document identity mismatch"
        );
        let services = doc["service"].as_array().context("Missing PDS service")?;
        let service = services
            .iter()
            .find(|s| s["id"] == format!("{did}#atproto_pds") || s["id"] == "#atproto_pds")
            .context("Missing PDS service")?;
        ensure!(
            service["type"] == "AtprotoPersonalDataServer",
            "Invalid PDS service"
        );
        let pds = public_url(
            service["serviceEndpoint"]
                .as_str()
                .context("Invalid PDS endpoint")?,
        )?;
        ensure!(pds.path() == "/", "PDS endpoint must be an origin");
        let methods = doc["verificationMethod"]
            .as_array()
            .context("Missing repository signing key")?;
        let method = methods
            .iter()
            .find(|v| v["id"] == format!("{did}#atproto") || v["id"] == "#atproto")
            .context("Missing repository signing key")?;
        ensure!(
            method["controller"] == did && method["type"] == "Multikey",
            "Invalid repository signing authority"
        );
        let key = method["publicKeyMultibase"]
            .as_str()
            .context("Invalid repository public key")?
            .to_string();
        ensure!(key.len() < 256, "Oversized repository key");
        Ok(Identity { pds, key })
    }
    async fn verify_at(
        &self,
        identity: &Identity,
        did: &str,
        endpoint: EndpointId,
    ) -> Result<VerifiedDevice> {
        let rkey = endpoint.to_string();
        let car = self
            .get(
                identity.pds.join("xrpc/com.atproto.sync.getRecord")?,
                &[("did", did), ("collection", COLLECTION), ("rkey", &rkey)],
                1024 * 1024,
            )
            .await?;
        let head = self
            .json(
                identity.pds.join("xrpc/com.atproto.sync.getLatestCommit")?,
                &[("did", did)],
            )
            .await?;
        let record = proof::verify(
            &car,
            did,
            &identity.key,
            head["cid"].as_str().context("Missing current commit")?,
            &format!("{COLLECTION}/{rkey}"),
        )?;
        verify_record(&record, did, endpoint)
    }
    pub async fn verify(&self, did: &str, endpoint: EndpointId) -> Result<VerifiedDevice> {
        self.verify_at(&self.identity(did).await?, did, endpoint)
            .await
    }
    pub async fn list(&self, did: &str) -> Result<Vec<VerifiedDevice>> {
        let identity = self.identity(did).await?;
        let response = self
            .json(
                identity.pds.join("xrpc/com.atproto.repo.listRecords")?,
                &[("repo", did), ("collection", COLLECTION), ("limit", "16")],
            )
            .await?;
        ensure!(
            response["cursor"].is_null(),
            "This account has more than 16 devices; use a Snapcode"
        );
        let records = response["records"]
            .as_array()
            .context("Missing device list")?;
        ensure!(records.len() <= 16, "Too many device records");
        let prefix = format!("at://{did}/{COLLECTION}/");
        let mut verified = Vec::new();
        for record in records {
            let rkey = record["uri"]
                .as_str()
                .and_then(|s| s.strip_prefix(&prefix))
                .context("Device record URI mismatch")?;
            ensure!(
                rkey.len() == 64
                    && rkey
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "Invalid device record key"
            );
            // Never fall back to an unverified endpoint if any proof fails.
            verified.push(self.verify_at(&identity, did, rkey.parse()?).await?);
        }
        Ok(verified)
    }
}

pub async fn publish(account: &Account, key: &SecretKey) -> Result<VerifiedDevice> {
    write_record(account, key).await?;
    Directory::new()?
        .verify(&account.profile.did, key.public())
        .await
        .context("Record written, but verification failed. Retry checking this device.")
}
pub(crate) async fn write_record(account: &Account, key: &SecretKey) -> Result<()> {
    let did = &account.profile.did;
    let input = json!({"repo":did,"collection":COLLECTION,"rkey":key.public().to_string(),
        "record":record(did,key)?,"validate":false});
    account
        .session
        .send_xrpc::<(), _, Value, Value>(&XrpcRequest {
            method: Method::POST,
            nsid: "com.atproto.repo.putRecord".into(),
            parameters: None,
            input: Some(atrium_xrpc::InputDataOrBytes::Data(input)),
            encoding: Some("application/json".into()),
        })
        .await
        .context("Could not publish device. Sign in again to grant device-record permission.")?;
    Ok(())
}
pub async fn revoke(account: &Account, endpoint: EndpointId) -> Result<()> {
    let input =
        json!({"repo":account.profile.did,"collection":COLLECTION,"rkey":endpoint.to_string()});
    account
        .session
        .send_xrpc::<(), _, Value, Value>(&XrpcRequest {
            method: Method::POST,
            nsid: "com.atproto.repo.deleteRecord".into(),
            parameters: None,
            input: Some(atrium_xrpc::InputDataOrBytes::Data(input)),
            encoding: Some("application/json".into()),
        })
        .await
        .context("Could not remove device record; check your connection or sign in again.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_signature_binds_issuer_endpoint_and_protocol() {
        let key = SecretKey::generate();
        let did = "did:plc:alice";
        let Ipld::Map(mut fields) = unsigned(did, key.public()) else {
            unreachable!()
        };
        let sig = key.sign(&proof::encode(&Ipld::Map(fields.clone())).unwrap());
        fields.insert("proof".into(), Ipld::Bytes(sig.to_bytes().to_vec()));
        let signed = Ipld::Map(fields.clone());
        assert!(verify_record(&signed, did, key.public()).is_ok());
        assert!(verify_record(&signed, "did:plc:mallory", key.public()).is_err());
        assert!(verify_record(&signed, did, SecretKey::generate().public()).is_err());
        fields.insert(
            "via".into(),
            Ipld::List(vec![Ipld::String("another/protocol".into())]),
        );
        assert!(verify_record(&Ipld::Map(fields), did, key.public()).is_err());
        let json = record(did, &key).unwrap();
        assert_eq!(json.as_object().unwrap().len(), 5);
        assert!(json.get("request_token").is_none());
        assert!(json.get("endpoint").is_none());
        assert_eq!(
            STANDARD_NO_PAD
                .decode(json["proof"]["$bytes"].as_str().unwrap())
                .unwrap()
                .len(),
            64
        );
    }
    #[test]
    fn rejects_private_directory_destinations() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.4.2",
            "192.168.0.1",
            "169.254.1.1",
            "100.64.1.1",
            "0.0.0.0",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for url in [
            "http://pds.example.com",
            "https://127.0.0.1",
            "https://[::1]",
            "https://user:pass@pds.example.com",
            "https://pds.example.com:9999",
            "https://pds.example.com/?x=1",
        ] {
            assert!(public_url(url).is_err(), "{url}");
        }
    }
    #[tokio::test]
    #[ignore = "read-only live network check against a public Bluesky repository"]
    async fn live_repository_inclusion() -> Result<()> {
        let profile = crate::discovery::Discovery::new()?
            .profile("bsky.app")
            .await?;
        let directory = Directory::new()?;
        let identity = directory.identity(&profile.did).await?;
        let car = directory
            .get(
                identity.pds.join("xrpc/com.atproto.sync.getRecord")?,
                &[
                    ("did", &profile.did),
                    ("collection", "app.bsky.actor.profile"),
                    ("rkey", "self"),
                ],
                1024 * 1024,
            )
            .await?;
        let head = directory
            .json(
                identity.pds.join("xrpc/com.atproto.sync.getLatestCommit")?,
                &[("did", &profile.did)],
            )
            .await?;
        let record = proof::verify(
            &car,
            &profile.did,
            &identity.key,
            head["cid"].as_str().context("No head")?,
            "app.bsky.actor.profile/self",
        )?;
        ensure!(
            proof::map(&record)?.get("$type")
                == Some(&Ipld::String("app.bsky.actor.profile".into())),
            "Incorrect proven record"
        );
        Ok(())
    }
}
