use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub id: String,
    pub name: String,
    pub handle: String,
    pub did: String,
    pub color: String,
    pub endpoint: Option<iroh::EndpointAddr>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Invite {
    pub version: u8,
    pub name: String,
    pub handle: String,
    pub did: String,
    pub endpoint: iroh::EndpointAddr,
    #[serde(default)]
    pub request_token: String,
}

impl Invite {
    pub fn encode(&self) -> Result<String> {
        Ok(format!(
            "flicker://friend/{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }
    pub fn decode(s: &str) -> Result<Self> {
        ensure!(s.len() < 16000, "Invitation is too long");
        let data = URL_SAFE_NO_PAD.decode(
            s.trim()
                .strip_prefix("flicker://friend/")
                .ok_or_else(|| anyhow::anyhow!("Paste a n0-snap invitation"))?,
        )?;
        let invite: Self = serde_json::from_slice(&data)?;
        ensure!(
            invite.version == 1
                && invite.name.chars().count() <= 80
                && invite.name.len() <= 320
                && invite.handle.len() < 256
                && invite.request_token.len() <= 128,
            "Unsupported invitation"
        );
        Ok(invite)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ticket {
    pub id: String,
    pub store: iroh::EndpointAddr,
    pub read_cap: String,
    pub key: String,
    pub nonce: String,
    pub expires_at: u64,
    pub mime: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Offer {
    pub id: String,
    pub caption: String,
    pub kind: String,
    pub ticket: Ticket,
    pub created_at: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: String,
    pub contact: String,
    pub caption: String,
    pub kind: String,
    pub created_at: u64,
    pub opened_at: Option<u64>,
    pub saved: bool,
    pub outgoing: bool,
    pub sample: Option<String>,
    pub ticket: Option<Ticket>,
    pub memory_cipher: Option<String>,
}

impl Item {
    pub fn expires_at(&self) -> u64 {
        if self.kind == "story" {
            self.created_at.saturating_add(86400)
        } else {
            self.opened_at
                .map(|t| t.saturating_add(60))
                .unwrap_or_else(|| self.created_at.saturating_add(86400))
        }
    }
    pub fn expired(&self, time: u64) -> bool {
        time >= self.expires_at()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    pub name: String,
    pub handle: String,
    pub host_mode: String,
    pub endpoint: String,
    pub write_token: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub contacts: Vec<Contact>,
    pub items: Vec<Item>,
    pub settings: Settings,
    #[serde(default)]
    pub discovery_actor: String,
    #[serde(default)]
    pub friend_requests: Vec<FriendRequest>,
    #[serde(default)]
    pub declined_requests: Vec<iroh::EndpointId>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FriendRequest {
    pub invite: Invite,
    pub outgoing: bool,
    #[serde(default)]
    pub declined: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            contacts: Vec::new(),
            items: Vec::new(),
            discovery_actor: String::new(),
            friend_requests: Vec::new(),
            declined_requests: Vec::new(),
            settings: Settings {
                name: "You".into(),
                handle: String::new(),
                host_mode: "local".into(),
                endpoint: String::new(),
                write_token: String::new(),
            },
        }
    }
}

impl State {
    pub fn connect_friend(&mut self, invite: &Invite) -> String {
        let id = invite.endpoint.id.to_string();
        if !self.contacts.iter().any(|c| c.id == id) {
            self.contacts.push(Contact {
                id: id.clone(),
                name: invite.name.clone(),
                handle: invite.handle.clone(),
                // v2 invitations are only constructed by the verified PDS flow;
                // external QR/deep links and the legacy wire protocol accept v1 only.
                did: if invite.version == 2 {
                    invite.did.clone()
                } else {
                    String::new()
                },
                color: "#7c7cff".into(),
                endpoint: Some(invite.endpoint.clone()),
            });
        }
        self.friend_requests
            .retain(|r| r.invite.endpoint.id != invite.endpoint.id);
        id
    }
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("state.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }
    pub fn save(&self, dir: &Path) -> Result<()> {
        let mut copy = self.clone();
        for item in &mut copy.items {
            if item.expired(now()) && !item.saved {
                item.ticket = None;
                item.sample = None;
                item.memory_cipher = None;
            }
        }
        private_write(&dir.join("state.tmp"), &serde_json::to_vec_pretty(&copy)?)?;
        std::fs::rename(dir.join("state.tmp"), dir.join("state.json"))?;
        Ok(())
    }
}

pub fn data_dir() -> PathBuf {
    std::env::var_os("FLICKER_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("flicker-prototype")
        })
}

pub fn account_dir(root: &Path, did: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    root.join("accounts")
        .join(hex::encode(Sha256::digest(did.as_bytes())))
}

pub fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample_item() -> Item {
        Item {
            id: uuid::Uuid::new_v4().to_string(),
            contact: "test".into(),
            caption: String::new(),
            kind: "snap".into(),
            created_at: now(),
            opened_at: None,
            saved: false,
            outgoing: false,
            sample: Some("coast".into()),
            ticket: None,
            memory_cipher: None,
        }
    }
    #[test]
    fn fresh_accounts_are_empty_and_isolated() -> Result<()> {
        let root = std::env::temp_dir().join(format!("flicker-accounts-{}", uuid::Uuid::new_v4()));
        let alice = account_dir(&root, "did:plc:alice");
        let bob = account_dir(&root, "did:plc:bob");
        assert_ne!(alice, bob);
        assert!(account_dir(&root, "../../other").starts_with(root.join("accounts")));
        let mut state = State::default();
        assert!(state.contacts.is_empty() && state.items.is_empty());
        state.items.push(sample_item());
        state.save(&alice)?;
        assert!(State::load(&bob)?.items.is_empty());
        assert_eq!(State::load(&alice)?.items.len(), 1);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn discovery_state_migrates_and_device_invites_do_not_verify_at_identity() -> Result<()> {
        let mut legacy = serde_json::to_value(State::default())?;
        for field in ["discovery_actor", "friend_requests", "declined_requests"] {
            legacy.as_object_mut().unwrap().remove(field);
        }
        let mut state: State = serde_json::from_value(legacy)?;
        assert!(state.friend_requests.is_empty());
        let key = iroh::SecretKey::generate();
        let invite = Invite {
            version: 1,
            name: "A person".into(),
            handle: "claimed.example".into(),
            did: "did:plc:unverified".into(),
            endpoint: key.public().into(),
            request_token: "test capability".into(),
        };
        state.friend_requests.push(FriendRequest {
            invite: invite.clone(),
            outgoing: true,
            declined: false,
        });
        state.discovery_actor = "network.example".into();
        let restored: State = serde_json::from_slice(&serde_json::to_vec(&state)?)?;
        assert_eq!(restored.friend_requests.len(), 1);
        assert_eq!(restored.discovery_actor, "network.example");
        let id = state.connect_friend(&invite);
        state.connect_friend(&invite);
        assert_eq!(state.contacts.iter().filter(|c| c.id == id).count(), 1);
        assert!(
            state
                .contacts
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .did
                .is_empty()
        );
        assert!(state.friend_requests.is_empty());
        // Codes from the first prototype remain readable.
        let mut old = serde_json::to_value(&invite)?;
        old.as_object_mut().unwrap().remove("request_token");
        let encoded = format!(
            "flicker://friend/{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&old)?)
        );
        assert!(Invite::decode(&encoded)?.request_token.is_empty());
        Ok(())
    }

    #[test]
    fn snap_deadline_does_not_slide_on_reopen() {
        let mut item = sample_item();
        item.opened_at = Some(100);
        assert!(!item.expired(159));
        assert!(item.expired(160));
        item.kind = "story".into();
        item.created_at = 100;
        assert!(!item.expired(86499));
        assert!(item.expired(86500));
    }

    #[test]
    fn restart_preserves_expiry_and_keeps_only_saved_samples() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("flicker-state-{}", uuid::Uuid::new_v4()));
        let mut state = State::default();
        state.items = vec![sample_item(), sample_item()];
        state.items[0].opened_at = Some(now() - 61);
        state.items[1].opened_at = Some(now() - 61);
        state.items[1].saved = true;
        state.save(&dir)?;
        let restored = State::load(&dir)?;
        assert!(restored.items[0].expired(now()));
        assert!(restored.items[0].sample.is_none());
        assert!(restored.items[1].sample.is_some());
        assert!(restored.items[1].saved);
        std::fs::remove_dir_all(dir)?;
        Ok(())
    }
}
