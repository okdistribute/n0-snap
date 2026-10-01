//! Public AT discovery; these results are not proof of n0-snap membership or identity.
use crate::auth::BlueskySession;
use anyhow::{Context, Result, ensure};
use atrium_api::agent::CloneWithProxy;
use atrium_xrpc::{OutputDataOrBytes, XrpcClient, XrpcRequest, http::Method};
use serde::Deserialize;
use std::sync::Arc;
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Profile {
    pub did: String,
    pub handle: String,
    #[serde(rename = "displayName", default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub avatar: Option<String>,
}
impl Profile {
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.handle
        } else {
            &self.name
        }
    }
}
#[derive(Clone, Default, Deserialize)]
pub struct SearchPage {
    pub actors: Vec<Profile>,
    pub cursor: Option<String>,
}
#[derive(Clone, Deserialize)]
pub struct FollowPage {
    pub subject: Profile,
    pub follows: Vec<Profile>,
    pub cursor: Option<String>,
}
#[derive(Clone, PartialEq)]
pub struct Suggestion {
    pub profile: Profile,
    pub via: Vec<String>,
}
#[derive(Clone)]
pub struct Discovery {
    client: reqwest::Client,
    session: Option<Arc<BlueskySession>>,
}
impl Discovery {
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: None,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()?,
        })
    }
    pub fn authenticated(session: Arc<BlueskySession>) -> Result<Self> {
        let mut api = Self::new()?;
        // Keep the original session pointed at the PDS for repo writes. Only
        // profile requests carry the AppView proxy header (no shared mutation).
        api.session = Some(Arc::new(session.clone_with_proxy(
            "did:web:api.bsky.app".parse().expect("AppView DID"),
            "bsky_appview",
        )));
        Ok(api)
    }
    async fn get<T: serde::de::DeserializeOwned + Send + Sync>(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<T> {
        if let Some(session) = &self.session {
            let params: std::collections::BTreeMap<_, _> = params.iter().copied().collect();
            let result = session.send_xrpc::<_, (), T, serde_json::Value>(&XrpcRequest {
                method: Method::GET, nsid: method.into(), parameters: Some(params), input: None, encoding: None,
            }).await.context("Could not load Bluesky profiles. Check your connection; if your session has expired, sign out and sign in again.")?;
            return match result {
                OutputDataOrBytes::Data(value) => Ok(value),
                _ => anyhow::bail!("Bluesky returned an unreadable response"),
            };
        }
        let response = self
            .client
            .get(format!("https://public.api.bsky.app/xrpc/{method}"))
            .query(params)
            .send()
            .await
            .context("Could not reach Bluesky. Check your connection and try again.")?;
        ensure!(
            response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS,
            "Bluesky is busy. Wait a moment and try again."
        );
        response
            .error_for_status()
            .context("Bluesky could not load these people. Check the handle or try again.")?
            .json()
            .await
            .context("Bluesky returned an unreadable response. Try again.")
    }
    pub async fn profile(&self, actor: &str) -> Result<Profile> {
        self.get(
            "app.bsky.actor.getProfile",
            &[("actor", actor.trim().trim_start_matches('@'))],
        )
        .await
    }
    pub async fn search(&self, query: &str, cursor: Option<&str>) -> Result<SearchPage> {
        let query = query.trim().trim_start_matches('@');
        ensure!(
            query.chars().count() >= 2,
            "Type at least two characters to find people."
        );
        let mut params = vec![("q", query), ("limit", "24")];
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor));
        }
        self.get("app.bsky.actor.searchActors", &params).await
    }
    pub async fn follows(&self, actor: &str, cursor: Option<&str>) -> Result<FollowPage> {
        let mut params = vec![
            ("actor", actor.trim().trim_start_matches('@')),
            ("limit", "100"),
        ];
        if let Some(cursor) = cursor {
            params.push(("cursor", cursor));
        }
        self.get("app.bsky.graph.getFollows", &params).await
    }
    /// A bounded sample of public follow edges, with explicit provenance on each card.
    pub async fn connections(&self, actor: &str) -> Result<(Vec<Suggestion>, usize)> {
        let page = self.follows(actor, None).await?;
        let mut tasks = tokio::task::JoinSet::new();
        for seed in page.follows.iter().take(6).cloned() {
            let api = self.clone();
            tasks.spawn(async move {
                let follows = api.follows(&seed.did, None).await?;
                anyhow::Ok((seed, follows.follows))
            });
        }
        let mut graphs = vec![];
        let mut failed = 0;
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(graph)) => graphs.push(graph),
                _ => failed += 1,
            }
        }
        ensure!(
            page.follows.is_empty() || !graphs.is_empty(),
            "Couldn't load public connections. Try again."
        );
        Ok((
            rank_connections(&page.subject.did, &page.follows, graphs),
            failed,
        ))
    }
}
fn rank_connections(
    subject: &str,
    follows: &[Profile],
    graphs: Vec<(Profile, Vec<Profile>)>,
) -> Vec<Suggestion> {
    let known: HashSet<_> = follows
        .iter()
        .map(|p| p.did.as_str())
        .chain(std::iter::once(subject))
        .collect();
    let mut candidates: HashMap<String, Suggestion> = HashMap::new();
    for (seed, profiles) in graphs {
        let mut seen = HashSet::new();
        for profile in profiles {
            if known.contains(profile.did.as_str()) || !seen.insert(profile.did.clone()) {
                continue;
            }
            let entry = candidates.entry(profile.did.clone()).or_insert(Suggestion {
                profile,
                via: vec![],
            });
            entry.via.push(format!("@{}", seed.handle));
        }
    }
    let mut result: Vec<_> = candidates.into_values().collect();
    for entry in &mut result {
        entry.via.sort();
        entry.via.dedup();
    }
    result.sort_by(|a, b| {
        b.via
            .len()
            .cmp(&a.via.len())
            .then(a.profile.handle.cmp(&b.profile.handle))
    });
    result.truncate(48);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p(id: &str) -> Profile {
        Profile {
            did: id.into(),
            handle: format!("{id}.test"),
            name: String::new(),
            description: String::new(),
            avatar: None,
        }
    }
    #[test]
    fn suggestions_exclude_known_people_and_rank_real_distinct_edges() {
        let a = p("a");
        let b = p("b");
        let results = rank_connections(
            "self",
            &[a.clone(), b.clone()],
            vec![
                (a, vec![p("self"), p("b"), p("x"), p("x"), p("y")]),
                (b, vec![p("x"), p("z")]),
            ],
        );
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].profile.did, "x");
        assert_eq!(results[0].via, vec!["@a.test", "@b.test"]);
        assert_eq!(results[1].profile.did, "y");
    }
    #[test]
    fn public_profiles_allow_missing_optional_details_and_unicode() {
        let profile: Profile = serde_json::from_str(
            r#"{"did":"did:plc:test","handle":"test.example","displayName":"山 🌻"}"#,
        )
        .unwrap();
        assert_eq!(profile.label(), "山 🌻");
        assert!(profile.avatar.is_none());
    }
}
