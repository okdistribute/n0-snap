mod discover_ui;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use dioxus::prelude::*;
use discover_ui::Discover;
use flicker::discovery::Profile as PublicProfile;
use flicker::{
    model::*,
    network::{self, Client, FriendStatus},
};
use std::{sync::Arc, time::Duration};

fn main() {
    if let Err(e) = State::load(&data_dir()) {
        eprintln!("Could not load n0-snap data: {e}. Your existing data was left in place.");
        std::process::exit(1);
    }
    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            dioxus::desktop::Config::new().with_window(
                dioxus::desktop::WindowBuilder::new()
                    .with_title("n0-snap")
                    .with_inner_size(dioxus::desktop::LogicalSize::new(1320.0, 860.0))
                    .with_min_inner_size(dioxus::desktop::LogicalSize::new(860.0, 650.0)),
            ),
        )
        .launch(App);
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Inbox,
    Stories,
    Memories,
    Friends,
    Discover,
    Hosting,
}
#[derive(Clone, PartialEq)]
struct Viewer {
    item: Item,
    uri: String,
    mime: String,
    cipher: Option<String>,
    deadline: Option<u64>,
}
#[derive(Clone, Copy)]
struct Ctx {
    state: Signal<State>,
    client: Signal<Option<Arc<Client>>>,
    page: Signal<Page>,
    selected: Signal<String>,
    toast: Signal<String>,
    viewer: Signal<Option<Viewer>>,
    compose: Signal<bool>,
    add: Signal<bool>,
    tick: Signal<u64>,
}

#[component]
fn App() -> Element {
    let mut state = use_signal(|| State::load(&data_dir()).expect("validated state"));
    let mut client = use_signal(|| None::<Arc<Client>>);
    let page = use_signal(|| Page::Inbox);
    let selected = use_signal(|| "mira".to_string());
    let mut toast = use_signal(String::new);
    let mut viewer = use_signal(|| None::<Viewer>);
    let compose = use_signal(|| false);
    let add = use_signal(|| false);
    let mut tick = use_signal(now);
    let mut cx = Ctx {
        state,
        client,
        page,
        selected,
        toast,
        viewer,
        compose,
        add,
        tick,
    };
    use_context_provider(|| cx);
    use_effect(move || {
        if let Err(e) = state.read().save(&data_dir()) {
            toast.set(format!("Could not save locally: {e}"));
        }
    });
    use_future(move || async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (friend_tx, mut friend_rx) = tokio::sync::mpsc::channel(32);
        match Client::start_with_requests(data_dir(), tx, friend_tx).await {
            Ok(node) => {
                let node = Arc::new(node);
                let contacts = state.read().contacts.clone();
                for contact in contacts {
                    if let Some(addr) = &contact.endpoint {
                        node.allowed.write().await.insert(addr.id);
                    }
                }
                node.declined
                    .write()
                    .await
                    .extend(state.read().declined_requests.iter().copied());
                client.set(Some(node));
                loop {
                    tokio::select! {
                        Some(invite) = friend_rx.recv() => {
                            if !state.read().friend_requests.iter().any(|r| !r.outgoing && r.invite.endpoint.id == invite.endpoint.id) && !state.read().contacts.iter().any(|c| c.endpoint.as_ref().is_some_and(|e| e.id == invite.endpoint.id)) {
                                toast.set(format!("{} wants to connect. Open Friends to review.", invite.name));
                                state.write().friend_requests.push(FriendRequest { invite, outgoing: false, declined: false });
                            }
                        }
                        Some((remote, offer)) = rx.recv() => {
                        let friend = state
                            .read()
                            .contacts
                            .iter()
                            .find(|c| c.endpoint.as_ref().is_some_and(|e| e.id == remote))
                            .cloned();
                        if let Some(friend) = friend {
                            if state.read().items.iter().any(|i| i.id == offer.id) {
                                continue;
                            }
                            state.write().items.push(Item {
                                id: offer.id,
                                contact: friend.id,
                                caption: offer.caption,
                                kind: offer.kind,
                                created_at: offer.created_at.min(now()),
                                opened_at: None,
                                saved: false,
                                outgoing: false,
                                sample: None,
                                ticket: Some(offer.ticket),
                                memory_cipher: None,
                            });
                            toast.set(format!("A new moment from {}", friend.name));
                        }
                    } }
                }
            }
            Err(e) => toast.set(format!("Could not start iroh: {e}")),
        }
    });
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(8)).await;
            let Some(node) = client.read().clone() else {
                continue;
            };
            let pending: Vec<_> = state
                .read()
                .friend_requests
                .iter()
                .filter(|r| r.outgoing && !r.declined)
                .take(32)
                .cloned()
                .collect();
            let mut checks = tokio::task::JoinSet::new();
            for request in pending {
                let node = node.clone();
                let own = own_invite(cx, &node);
                checks.spawn(async move {
                    let result = node.request_friend(&request.invite, own).await;
                    (request, result)
                });
            }
            while let Some(Ok((request, result))) = checks.join_next().await {
                if matches!(result, Ok(FriendStatus::Declined)) {
                    if let Some(pending) =
                        state.write().friend_requests.iter_mut().find(|r| {
                            r.outgoing && r.invite.endpoint.id == request.invite.endpoint.id
                        })
                    {
                        pending.declined = true;
                    }
                }
                if matches!(result, Ok(FriendStatus::Accepted))
                    && state
                        .read()
                        .friend_requests
                        .iter()
                        .any(|r| r.outgoing && r.invite.endpoint.id == request.invite.endpoint.id)
                {
                    state.write().connect_friend(&request.invite);
                    node.allowed
                        .write()
                        .await
                        .insert(request.invite.endpoint.id);
                    toast.set(format!(
                        "{} accepted. You're connected!",
                        request.invite.name
                    ));
                }
            }
        }
    });
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            tick.set(now());
            if viewer
                .read()
                .as_ref()
                .is_some_and(|v| v.deadline.is_some_and(|d| now() >= d))
            {
                viewer.set(None);
            }
            let needs_cleanup = state.read().items.iter().any(|i| {
                i.expired(now()) && !i.saved && (i.ticket.is_some() || i.sample.is_some())
            });
            if needs_cleanup {
                for item in &mut state.write().items {
                    if item.expired(now()) && !item.saved {
                        item.ticket = None;
                        item.sample = None;
                        item.memory_cipher = None;
                    }
                }
            }
        }
    });
    let title = match page() {
        Page::Inbox => "Good moments. Zero forever.",
        Page::Stories => "Today looks good on you.",
        Page::Memories => "Some things hit different.",
        Page::Friends => "Find your frequency.",
        Page::Discover => "Your next favorite people.",
        Page::Hosting => "Your space. Your rules.",
    };
    rsx! {
        style { {include_str!("../assets/style.css")} }
        div { class:"app-shell",
            Sidebar {}
            main { class:"workspace",
                header { class:"topbar",
                    div { class:"breadcrumb", span {class:"header-cross","✳"} "PRIVATE MOMENTS / PUBLIC CONNECTIONS" }
                    div { class:"topbar-right",
                        span { class:"connection", span { class:if client.read().is_some(){"dot"}else{"dot pending"} } if client.read().is_some(){"iroh connected"}else{"connecting…"} }
                        button { class:"profile-button", title:"Profile and Snapcode", onclick:move |_|cx.page.set(Page::Friends), "Y" }
                    }
                }
                section { class:"page-intro",
                    div { class:"intro-copy", p {class:"eyebrow",span{class:"eyebrow-dash"} "SEND IT. FEEL IT. LET IT GO."} h1 {"{title}"} p {class:"muted", "Your people. Your moments. A whole lot of right now."} }
                    div {class:"hero-orbit","aria-hidden":"true",span{class:"orbit-track"}span{class:"orbit-track second"}span{class:"orbit-number","60"}span{class:"orbit-unit","SECONDS"}span{class:"orbit-star","✳"}}
                    button { class:"primary new-snap", onclick:move |_|cx.compose.set(true), Icon{name:"plus"} "New snap" }
                }
                match page() {
                    Page::Inbox=>rsx!{ StoryStrip{} Inbox{} },
                    Page::Stories=>rsx!{ Gallery{memories:false} },
                    Page::Memories=>rsx!{ Gallery{memories:true} },
                    Page::Friends=>rsx!{ Friends{} },
                    Page::Discover=>rsx!{ Discover{} },
                    Page::Hosting=>rsx!{ Hosting{} },
                }
                footer { class:"page-footer", span {Icon{name:"spark"} "A little less forever. A lot more now."} span {"BUILT WITH IROH / EXPERIMENT 001"} }
            }
        }
        if !toast.read().is_empty() { div {class:"toast", role:"status", "{toast}" button {class:"icon-button", title:"Dismiss", onclick:move |_|toast.set(String::new()), Icon{name:"close"}}} }
        if compose() { Composer{} }
        if add() { AddFriend{} }
        if viewer.read().is_some() { SnapViewer{} }
    }
}

#[component]
fn Sidebar() -> Element {
    let mut cx = use_context::<Ctx>();
    let nav = [
        (Page::Inbox, "inbox", "Inbox"),
        (Page::Stories, "stories", "Stories"),
        (Page::Memories, "bookmark", "Memories"),
        (Page::Discover, "search", "Discover"),
        (Page::Friends, "people", "Friends"),
    ];
    let unread = cx
        .state
        .read()
        .items
        .iter()
        .filter(|i| !i.outgoing && i.kind != "story" && i.opened_at.is_none() && !i.expired(now()))
        .count();
    rsx! {
        aside {class:"sidebar",
            a {class:"wordmark", href:"#", onclick:move |_|cx.page.set(Page::Inbox), span {class:"brand-symbol",Icon{name:"spark"}} "n0-snap" span {class:"brand-period","."} }
            p {class:"sidebar-tagline","HERE. THEN GONE."}
            nav { for (page,icon,label) in nav { button {class:if (cx.page)()==page{"nav-item active"}else{"nav-item"},onclick:move |_|cx.page.set(page),Icon{name:icon} "{label}" if page==Page::Inbox&&unread>0 {span {class:"nav-count","{unread}"}} if page==Page::Friends { {let count=cx.state.read().friend_requests.iter().filter(|r|!r.outgoing).count();rsx!{if count>0{span{class:"nav-count","{count}"}}}} } } } }
            div {class:"sidebar-bottom",
                div {class:"sixty-note",span{class:"note-spark","✳"}span {class:"sixty-number","60"} span {"seconds." br{} "make them count."} p {"No forever required."} }
                button {class:if (cx.page)()==Page::Hosting{"nav-item active"}else{"nav-item"},onclick:move |_|cx.page.set(Page::Hosting),Icon{name:"cloud"} "Your hosting"}
                div {class:"local-profile",div {class:"avatar you","Y"} div {strong {"{cx.state.read().settings.name}"} small {"Local profile · sign-in soon"}} }
            }
        }
    }
}

#[component]
fn StoryStrip() -> Element {
    let mut cx = use_context::<Ctx>();
    let stories: Vec<_> = cx
        .state
        .read()
        .items
        .iter()
        .filter(|i| i.kind == "story" && !i.expired((cx.tick)()))
        .cloned()
        .collect();
    rsx! {
        section {class:"stories-strip",
            div {class:"section-row",h2 {"In the loop"} button {class:"text-button",onclick:move |_|cx.page.set(Page::Stories),"All stories" Icon{name:"arrow"}} }
            div {class:"story-circles",
                button {class:"story-circle add-story",onclick:move |_|cx.compose.set(true),span {class:"circle-inner",Icon{name:"plus"}} span {"Your story"} }
                for item in stories.into_iter().take(8) {
                    {let c=cx.state.read().contacts.iter().find(|c|c.id==item.contact).cloned();let label=if item.outgoing{"You".into()}else{c.map(|c|c.name.split_whitespace().next().unwrap_or("Friend").to_string()).unwrap_or("Friend".into())};let thumb=item.sample.as_deref().map(sample_uri).unwrap_or_else(||sample_uri("flowers"));
                    rsx! {button {class:"story-circle",onclick:move |_|open_item(cx,item.clone(),false),span {class:"story-ring",img {src:"{thumb}",alt:""}} span {"{label}"} }} }
                }
                p {class:"stories-caption", span{class:"mini-star","✳"} "A little window into their world." br{} span{class:"mono","24 HOURS. ZERO PRESSURE."} }
            }
        }
    }
}

#[component]
fn Inbox() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut filter = use_signal(String::new);
    let mut unopened = use_signal(|| false);
    let contacts: Vec<_> = cx
        .state
        .read()
        .contacts
        .iter()
        .filter(|c| c.endpoint.is_some() || c.did.is_empty())
        .filter(|c| {
            format!("{} {}", c.name, c.handle)
                .to_lowercase()
                .contains(&filter().to_lowercase())
        })
        .cloned()
        .collect();
    rsx! {
        section {class:"inbox-panel",
            aside {class:"conversation-list",
                div {class:"list-title",h2 {"Inbox"}button {class:"icon-button",title:"Add a friend",onclick:move |_|cx.add.set(true),Icon{name:"edit"}} }
                div {class:"search",Icon{name:"search"} input {placeholder:"Find a friend",value:"{filter}",oninput:move |e|filter.set(e.value())} }
                div {class:"filter-row",button {class:if !unopened(){"filter active"}else{"filter"},onclick:move |_|unopened.set(false),"All"}button {class:if unopened(){"filter active"}else{"filter"},onclick:move |_|unopened.set(true),"Unopened"} }
                for contact in contacts {
                    {let latest=cx.state.read().items.iter().rev().find(|i|i.contact==contact.id&&i.kind!="story").cloned();
                    let fresh=latest.as_ref().is_some_and(|i|!i.outgoing&&i.opened_at.is_none()&&!i.expired((cx.tick)()));
                    let label=match &latest{Some(i) if i.expired((cx.tick)())=>"Moment passed".into(),Some(i) if i.outgoing=>if contact.endpoint.is_none(){"Demo · stored on your host".into()}else{"Delivered over iroh".into()},Some(i) if i.opened_at.is_some()=>"Opened".into(),Some(_)=>"New snap · 60 sec".into(),None=>"Say a little hello".to_string()};
                    let age=latest.as_ref().map(|i|relative(i.created_at)).unwrap_or_default();let id=contact.id.clone();
                    rsx! {if !unopened()||fresh {button {class:if (cx.selected)()==contact.id{"conversation selected"}else{"conversation"},onclick:move |_|cx.selected.set(id.clone()),Avatar{contact:contact.clone()}div {class:"conversation-copy",div {strong {"{contact.name}"}span {class:"time","{age}"}}p {class:if fresh{"fresh"}else{""},if fresh {span {class:"small-square"}}"{label}"}}}}} }
                }
                div {class:"demo-note",span {class:"tiny-label","TRY IT OUT"}p {"Sample friends are here to explore. Add a real friend's Snapcode to connect."} }
            }
            Conversation{}
        }
    }
}

#[component]
fn Conversation() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut draft = use_signal(String::new);
    let mut sending = use_signal(|| false);
    let contact = cx
        .state
        .read()
        .contacts
        .iter()
        .find(|c| c.id == (cx.selected)())
        .cloned();
    let Some(contact) = contact else {
        return rsx! {div {class:"empty",h2{"A little hello goes a long way."}p{"Pick a friend to get started."}}};
    };
    let items: Vec<_> = cx
        .state
        .read()
        .items
        .iter()
        .filter(|i| i.contact == contact.id && i.kind != "story")
        .cloned()
        .collect();
    let for_send = contact.clone();
    let demo = contact.endpoint.is_none() && contact.did.is_empty();
    rsx! {
        div {class:"conversation-detail",
            header {class:"chat-header",Avatar{contact:contact.clone()}div {strong {"{contact.name}"}p {"@{contact.handle}"}}span {class:"chip",if demo{"SAMPLE FRIEND"}else if contact.endpoint.is_some(){"IROH PEER"}else{"NEEDS SNAPCODE"}} }
            div {class:"chat-scroll",
                div {class:"day-marker","TODAY"}
                div {class:"conversation-greeting",span {class:"greeting-orbit",Icon{name:"spark"}}h3 {"A moment between you two."}p {"Open it. Be there. Let it go."} }
                for item in items {
                    {let expired=item.expired((cx.tick)());let opened=item.opened_at.is_some();let out=item.outgoing;let label=if expired{"This moment has passed"}else if item.kind=="text"{"A little message"}else if item.ticket.as_ref().is_some_and(|t|t.mime.starts_with("video")){"A video for you"}else{"A little glimpse"};
                    let view=item.clone();
                    rsx! {div {class:if out{"message-row outgoing"}else{"message-row"},
                        div {class:if expired{"snap-card expired-card"}else{"snap-card"},
                            div {class:"snap-illustration",Icon{name:if item.kind=="text"{"inbox"}else{"camera"}}span {class:"spark-one","✳"}span {class:"spark-two","✳"}span {class:"snap-tag","60 SEC"}}
                            div {class:"snap-card-body",strong {"{label}"}p {if out{"Sent by you"}else if demo{"A sample moment from a friend"}else{"Just for your eyes. And your Memories."}}
                                button {class:"open-button",disabled:expired,onclick:move |_|open_item(cx,view.clone(),false),if expired{"Expired"}else if opened{"Continue viewing"}else{"Open snap"}Icon{name:"arrow"}}
                            }
                        }
                        p {class:"message-caption",if out{"You · "}else{"Received · "}"{relative(item.created_at)}" if item.saved{" · Saved to Memories"}}
                    }}}
                }
            }
            div {class:"chat-bottom",div {class:"privacy-line",Icon{name:"lock"}"Private over iroh. Disappearing unless saved."}
                form {class:"message-input",onsubmit:move |_|{
                    if draft().trim().is_empty()||sending(){return;}
                    let bytes=draft().as_bytes().to_vec();let c=for_send.clone();sending.set(true);
                    spawn(async move{match publish(cx,bytes,"text/plain".into(),"A little message".into(),"text".into(),vec![c]).await{Ok(())=>draft.set(String::new()),Err(e)=>cx.toast.set(e.to_string())}sending.set(false);});
                },button {r#type:"button",class:"icon-button",title:"Send a photo or video",onclick:move |_|cx.compose.set(true),Icon{name:"plus"}}
                input {placeholder:"Send a little something…",value:"{draft}",oninput:move |e|draft.set(e.value()),maxlength:2000}
                button {r#type:"submit",class:"send-button",disabled:sending(),title:"Send message",Icon{name:if sending(){"clock"}else{"arrow"}}}}
            }
        }
    }
}

#[component]
fn Gallery(memories: bool) -> Element {
    let mut cx = use_context::<Ctx>();
    let mut seen = std::collections::HashSet::new();
    let items: Vec<_> = cx
        .state
        .read()
        .items
        .iter()
        .filter(|i| {
            if memories {
                i.saved
            } else {
                i.kind == "story" && !i.expired((cx.tick)())
            }
        })
        .filter(|i| {
            seen.insert(
                i.ticket
                    .as_ref()
                    .map(|t| t.id.clone())
                    .unwrap_or(i.id.clone()),
            )
        })
        .cloned()
        .collect();
    rsx! {section {class:"gallery-section",
        div {class:"section-row",h2 {if memories{"Your private collection"}else{"Today, through their eyes"}}span {class:"muted",if memories{"Saved on this device"}else{"Available for 24 hours"}} }
        if items.is_empty(){div {class:"large-empty",Icon{name:if memories{"bookmark"}else{"stories"}}h2 {if memories{"Some moments are keepers."}else{"Today is a blank canvas."}}p {if memories{"Open a snap and choose Save to Memories. You'll find it here."}else{"Share a little of your day with a story."}}button {class:"primary",onclick:move |_|{if memories{cx.page.set(Page::Inbox)}else{cx.compose.set(true)}},if memories{"Explore your inbox"}else{"Add a story"}}}}
        div {class:"gallery-grid",for item in items {
            {let contact=cx.state.read().contacts.iter().find(|c|c.id==item.contact).cloned();let name=if item.outgoing{"You".into()}else{contact.map(|c|c.name).unwrap_or("Friend".into())};let img=item.sample.as_deref().map(sample_uri);let open=item.clone();let remove=item.id.clone();
            rsx!{article {class:"gallery-card",button {class:"gallery-cover",onclick:move |_|open_item(cx,open.clone(),memories),if let Some(img)=img{img{src:"{img}",alt:""}}else{div {class:"media-placeholder",Icon{name:"camera"}}}span{class:"gallery-shade"}span{class:"gallery-overline",if memories{"KEPT BY YOU"}else{"{relative(item.created_at)}"}}span{class:"gallery-title","{item.caption}"}span{class:"gallery-name","{name}"}}if memories {button {class:"delete-memory",onclick:move |_|{if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==remove){item.saved=false;item.memory_cipher=None;}cx.toast.set("Removed from Memories".into());},Icon{name:"close"}"Remove"}}}}
            }
        }}
    }}
}

fn sample_bytes(name: &str) -> &'static [u8] {
    match name {
        "mountain" => include_bytes!("../assets/mountain.jpg"),
        "flowers" => include_bytes!("../assets/flowers.jpg"),
        _ => include_bytes!("../assets/coast.jpg"),
    }
}
fn sample_uri(name: &str) -> String {
    format!(
        "data:image/jpeg;base64,{}",
        STANDARD.encode(sample_bytes(name))
    )
}
fn relative(t: u64) -> String {
    let d = now().saturating_sub(t);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else {
        format!("{}h ago", d / 3600)
    }
}

fn open_item(mut cx: Ctx, item: Item, memory: bool) {
    if item.expired(now()) && !memory {
        cx.toast.set("This moment has passed.".into());
        return;
    }
    spawn(async move {
        let result: Result<(String, String, Option<String>)> = async {
            if let Some(sample) = &item.sample {
                return Ok((sample_uri(sample), "image/jpeg".into(), None));
            }
            let ticket = item
                .ticket
                .as_ref()
                .context("This moment is no longer available")?;
            let cipher = if memory {
                item.memory_cipher
                    .clone()
                    .context("No local copy in Memories")?
            } else {
                let node = cx
                    .client
                    .read()
                    .clone()
                    .context("iroh is still connecting")?;
                node.download_cipher(ticket).await?
            };
            let data = network::decrypt(ticket, &cipher)?;
            let uri = if ticket.mime == "text/plain" {
                String::from_utf8(data)?
            } else {
                format!("data:{};base64,{}", ticket.mime, STANDARD.encode(data))
            };
            Ok((uri, ticket.mime.clone(), Some(cipher)))
        }
        .await;
        match result {
            Ok((uri, mime, cipher)) => {
                let start = item.opened_at.unwrap_or_else(now);
                let deadline = if memory {
                    None
                } else if item.kind == "story" {
                    Some((now() + 60).min(item.expires_at()))
                } else {
                    Some(
                        (start + 60).min(
                            item.ticket
                                .as_ref()
                                .map(|t| t.expires_at)
                                .unwrap_or(u64::MAX),
                        ),
                    )
                };
                if deadline.is_some_and(|d| d <= now()) {
                    cx.toast.set("This moment has passed.".into());
                    return;
                }
                if !memory {
                    if let Some(stored) =
                        cx.state.write().items.iter_mut().find(|i| i.id == item.id)
                    {
                        stored.opened_at.get_or_insert(start);
                    }
                }
                cx.viewer.set(Some(Viewer {
                    item,
                    uri,
                    mime,
                    cipher,
                    deadline,
                }));
            }
            Err(e) => cx.toast.set(e.to_string()),
        }
    });
}

#[component]
fn SnapViewer() -> Element {
    let mut cx = use_context::<Ctx>();
    let Some(view) = cx.viewer.read().clone() else {
        return rsx! {};
    };
    let remaining = view.deadline.map(|d| d.saturating_sub((cx.tick)()));
    let saved = cx
        .state
        .read()
        .items
        .iter()
        .find(|i| i.id == view.item.id)
        .is_some_and(|i| i.saved);
    let for_save = view.clone();
    rsx! {div {class:"viewer-backdrop",role:"dialog","aria-modal":"true","aria-label":"Snap viewer",
        div {class:"viewer-top",span{class:"viewer-logo",Icon{name:"spark"}"n0-snap."}span{class:"viewer-timer",Icon{name:"clock"}if let Some(s)=remaining{"{s}s"}else{"In your Memories"}}button {class:"viewer-close",title:"Close snap",onclick:move |_|{if view.item.kind!="story"&&view.deadline.is_some(){if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==view.item.id){item.opened_at=Some(now().saturating_sub(60));}}cx.viewer.set(None);},Icon{name:"close"}}}
        if let Some(s)=remaining {div {class:"viewer-progress",div {style:"width:{s as f64 / 60.0 * 100.0}%"}}}
        div {class:"viewer-media",
            if view.mime.starts_with("video/"){video {src:"{view.uri}",autoplay:true,controls:true,playsinline:true}}
            else if view.mime=="text/plain"{div {class:"text-snap","{view.uri}"}}
            else {img {src:"{view.uri}",alt:"Snap content"}}
            div {class:"viewer-caption","{view.item.caption}"}
        }
        div {class:"viewer-bottom",p {if saved{"Kept on this device."}else{"Here for a moment. Yours to keep if you choose."}}button {class:"save-memory",disabled:saved,onclick:move |_|{if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==for_save.item.id){item.saved=true;item.memory_cipher=for_save.cipher.clone();}cx.toast.set("Saved to Memories on this device".into());},Icon{name:"bookmark"}if saved{"Saved to Memories"}else{"Save to Memories"}}}
    }}
}

async fn publish(
    mut cx: Ctx,
    bytes: Vec<u8>,
    mime: String,
    caption: String,
    kind: String,
    contacts: Vec<Contact>,
) -> Result<()> {
    ensure!(
        contacts
            .iter()
            .all(|c| c.endpoint.is_some() || c.did.is_empty()),
        "Exchange Snapcodes with this profile before sending"
    );
    let node = cx
        .client
        .read()
        .clone()
        .context("iroh is still connecting")?;
    let settings = cx.state.read().settings.clone();
    let (addr, token) = host_target(&node, &settings)?;
    let ticket = node.upload(&bytes, &mime, addr, token).await?;
    let all_demo = contacts.iter().all(|c| c.endpoint.is_none());
    let mut failures = Vec::new();
    let mut delivered = 0;
    for contact in contacts {
        let offer = Offer {
            id: uuid::Uuid::new_v4().to_string(),
            caption: caption.clone(),
            kind: kind.clone(),
            ticket: ticket.clone(),
            created_at: now(),
        };
        if let Some(addr) = &contact.endpoint {
            if let Err(e) = node.send(addr.clone(), &offer).await {
                failures.push(format!("{}: {e}", contact.name));
                continue;
            }
        }
        cx.state.write().items.push(Item {
            id: offer.id,
            contact: contact.id,
            caption: caption.clone(),
            kind: kind.clone(),
            created_at: now(),
            opened_at: None,
            saved: false,
            outgoing: true,
            sample: None,
            ticket: Some(ticket.clone()),
            memory_cipher: None,
        });
        delivered += 1;
    }
    if !failures.is_empty() {
        cx.toast
            .set(format!("Sent to {delivered}. {}", failures.join("; ")));
        ensure!(delivered > 0, "{}", failures.join("; "));
    } else {
        cx.toast.set(if kind == "story" {
            "Story shared · available for 24 hours".into()
        } else {
            if all_demo {
                "Demo moment stored over iroh. Open it in this conversation.".into()
            } else {
                "Moment delivered over iroh.".into()
            }
        });
    }
    Ok(())
}

fn host_target(node: &Client, settings: &Settings) -> Result<(iroh::EndpointAddr, String)> {
    if settings.host_mode == "local" {
        Ok((
            node.local_store.endpoint.addr(),
            node.local_store.token.clone(),
        ))
    } else {
        ensure!(
            !settings.endpoint.trim().is_empty(),
            "Add your hosting endpoint in Your hosting first"
        );
        Ok((
            settings.endpoint.trim().parse::<iroh::EndpointId>()?.into(),
            settings.write_token.clone(),
        ))
    }
}

#[component]
fn Composer() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut chosen = use_signal(|| None::<(Vec<u8>, String, String)>);
    let mut caption = use_signal(String::new);
    let mut kind = use_signal(|| "snap".to_string());
    let mut recipient = use_signal(|| (cx.selected)());
    let mut busy = use_signal(|| false);
    let mut error = use_signal(String::new);
    let contacts: Vec<_> = cx
        .state
        .read()
        .contacts
        .iter()
        .filter(|c| c.endpoint.is_some() || c.did.is_empty())
        .cloned()
        .collect();
    let preview = chosen
        .read()
        .as_ref()
        .map(|(b, m, _)| format!("data:{m};base64,{}", STANDARD.encode(b)));
    rsx! {div {class:"modal-backdrop",div {class:"modal composer",role:"dialog","aria-modal":"true","aria-label":"New snap",
        div {class:"modal-heading",div{p{class:"eyebrow","A LITTLE SOMETHING"}h2{"Make their day."}}button{class:"icon-button",title:"Close",disabled:busy(),onclick:move |_|cx.compose.set(false),Icon{name:"close"}}}
        div {class:"compose-tabs",button{class:if kind()=="snap"{"active"}else{""},onclick:move |_|kind.set("snap".into()),"Direct snap"}button{class:if kind()=="story"{"active"}else{""},onclick:move |_|kind.set("story".into()),"24-hour story"}}
        button {class:"upload-area",disabled:busy(),onclick:move |_|{spawn(async move{if let Some(file)=rfd::AsyncFileDialog::new().add_filter("Photos & videos",&["jpg","jpeg","png","webp","gif","mp4","webm","mov"]).pick_file().await{
            if let Ok(meta)=std::fs::metadata(file.path()){if meta.len()>network::MAX_MEDIA as u64{error.set("Choose a file smaller than 12 MB".into());return;}}
            let name=file.file_name();let ext=file.path().extension().and_then(|s|s.to_str()).unwrap_or("").to_lowercase();let mime=match ext.as_str(){"jpg"|"jpeg"=>"image/jpeg","png"=>"image/png","gif"=>"image/gif","webp"=>"image/webp","mp4"=>"video/mp4","webm"=>"video/webm","mov"=>"video/quicktime",_=>""};
            if mime.is_empty(){error.set("Unsupported media type".into());return;}let data=file.read().await;if data.len()>network::MAX_MEDIA{error.set("Choose a file smaller than 12 MB".into());return;}chosen.set(Some((data,mime.into(),name)));error.set(String::new());
        }});},
            if let Some(uri)=preview {if chosen.read().as_ref().is_some_and(|(_,m,_)|m.starts_with("video")){video{src:"{uri}",muted:true}}else{img{src:"{uri}",alt:"Your selected photo"}}span{class:"replace-media","Choose something else"}}
            else {span{class:"upload-icon",Icon{name:"camera"}}strong{"A photo. A video. A little you."}p{"Choose from your device · up to 12 MB"}}
        }
        if chosen.read().is_none(){div{class:"sample-picker",span{"Or try a sample"}for sample in ["coast","mountain","flowers"]{button{title:"Use {sample} sample",onclick:move |_|chosen.set(Some((sample_bytes(sample).to_vec(),"image/jpeg".into(),format!("{sample}.jpg")))),img{src:"{sample_uri(sample)}",alt:"{sample}"}}}}}
        label {class:"field-label","A few words, if you want"}input{class:"field",placeholder:"Wish you were here…",value:"{caption}",maxlength:2000,oninput:move |e|caption.set(e.value())}
        if kind()=="snap"{label{class:"field-label","For"}select{class:"field",value:"{recipient}",onchange:move |e|recipient.set(e.value()),for contact in contacts{option{value:"{contact.id}","{contact.name}" if contact.endpoint.is_none()&&contact.did.is_empty(){" (sample)"}else if contact.endpoint.is_none(){" (needs Snapcode)"}}}}}else{p{class:"form-hint","Shared with all added friends. Real peers must be online to receive the story invitation; media stays on your host."}}
        if !error().is_empty(){p{class:"form-error",role:"alert","{error}"}}
        div{class:"modal-footer",span{class:"form-hint",if kind()=="snap"{"60 seconds to view. Can be saved."}else{"Visible for 24 hours."}}button{class:"primary",disabled:chosen.read().is_none()||busy(),onclick:move |_|{
            let Some((bytes,mime,_))=chosen().clone()else{return;};let contacts:Vec<_>=cx.state.read().contacts.iter().filter(|c|if kind()=="story"{c.endpoint.is_some()||c.did.is_empty()}else{c.id==recipient()}).cloned().collect();
            if contacts.is_empty(){error.set("Add a friend first".into());return;}busy.set(true);error.set(String::new());let caption=caption();let kind=kind();
            spawn(async move{match publish(cx,bytes,mime,caption,kind,contacts).await{Ok(())=>cx.compose.set(false),Err(e)=>error.set(e.to_string())}busy.set(false);});
        },if busy(){"Sending over iroh…"}else{"Send a moment"}Icon{name:"arrow"}}}
    }}}
}

fn own_invite(cx: Ctx, node: &Client) -> Invite {
    Invite {
        version: 1,
        name: cx.state.read().settings.name.clone(),
        handle: String::new(),
        did: String::new(),
        endpoint: node.endpoint.addr(),
        request_token: node.request_token.clone(),
    }
}

#[component]
fn FriendRequests() -> Element {
    let mut cx = use_context::<Ctx>();
    let requests = cx.state.read().friend_requests.clone();
    rsx! {
        if !requests.is_empty() {
            section { class:"request-panel",
                div { class:"section-row",h2 {"Connection requests"}span {class:"chip","YOU CHOOSE WHO GETS IN"} }
                for request in requests {
                    { let invite=request.invite.clone();let accept=invite.clone();let remove=invite.clone();
                    rsx! {div {class:"request-row",key:"{invite.endpoint.id}-{request.outgoing}",
                        div {class:"avatar you",Icon{name:"people"}}
                        div {class:"request-copy",strong {"{invite.name}"}p {class:"muted",if request.declined {"Request declined"}else if request.outgoing {"Waiting for acceptance · retries while this app is open"}else{"Wants to connect · device identity only"}}small {"Device {invite.endpoint.id}"}}
                        if !request.outgoing {button {class:"primary",disabled:cx.client.read().is_none(),onclick:move |_| {
                            let Some(node)=cx.client.read().clone() else {return;};
                            let id=accept.endpoint.id;
                            cx.state.write().connect_friend(&accept);
                            cx.state.write().declined_requests.retain(|e|*e!=id);
                            spawn(async move {node.declined.write().await.remove(&id);node.allowed.write().await.insert(id);});
                            cx.toast.set("Accepted. Their app will connect automatically on its next check.".into());
                        },"Accept"}}
                        button {class:"secondary",onclick:move |_| {
                            let id=remove.endpoint.id;
                            cx.state.write().friend_requests.retain(|r|!(r.invite.endpoint.id==id&&r.outgoing==request.outgoing));
                            if !request.outgoing {
                                cx.state.write().declined_requests.push(id);
                                if let Some(node)=cx.client.read().clone(){spawn(async move{node.declined.write().await.insert(id);});}
                            }
                        },if request.outgoing {"Cancel"}else{"Decline"}}
                    }} }
                }
            }
        }
    }
}

#[component]
fn Friends() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut name = use_signal(|| cx.state.read().settings.name.clone());
    let mut show_code = use_signal(|| false);
    let invite = cx
        .client
        .read()
        .as_ref()
        .and_then(|node| {
            Invite {
                version: 1,
                name: cx.state.read().settings.name.clone(),
                handle: cx.state.read().settings.handle.clone(),
                did: String::new(),
                endpoint: node.endpoint.addr(),
                request_token: node.request_token.clone(),
            }
            .encode()
            .ok()
        })
        .unwrap_or_default();
    let svg = if show_code() {
        qrcode::QrCode::new(invite.as_bytes())
            .ok()
            .map(|q| {
                q.render::<qrcode::render::svg::Color>()
                    .min_dimensions(256, 256)
                    .dark_color(qrcode::render::svg::Color("#18181b"))
                    .light_color(qrcode::render::svg::Color("#ffffff"))
                    .build()
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let friends: Vec<_> = cx
        .state
        .read()
        .contacts
        .iter()
        .filter(|c| c.endpoint.is_some() || c.did.is_empty())
        .cloned()
        .collect();
    rsx! {section {class:"friends-page",
        FriendRequests {}
        div{class:"friend-intro",div{h2{"Already your people."}p{class:"muted","Browse Discover for new people, or use a Snapcode to request a connection."}}button{class:"primary",onclick:move |_|cx.add.set(true),Icon{name:"plus"}"Add a friend"}}
        div{class:"friend-grid",for contact in friends{div{class:"friend-card",Avatar{contact:contact.clone()}h3{"{contact.name}"}p{if contact.handle.is_empty(){"Connected device"}else{"@{contact.handle}"}}span{class:"chip",if contact.endpoint.is_some(){"IROH CONTACT"}else if contact.did.is_empty(){"SAMPLE PROFILE"}else{"NEEDS SNAPCODE"}}button{class:"text-button",onclick:move |_|{cx.selected.set(contact.id.clone());cx.page.set(Page::Inbox);},"Open conversation" Icon{name:"arrow"}}}}}
        div{class:"identity-card",div{p{class:"eyebrow","YOUR SNAPCODE"}h2{"An invitation to your corner."}p{class:"muted","Share your code so a friend can request a connection. You choose who gets in. This verifies a device, not an AT Protocol account."}
        label{class:"field-label","Your name on this device"}div{class:"inline-field",input{class:"field",value:"{name}",oninput:move |e|name.set(e.value()),maxlength:80}button{class:"secondary",onclick:move |_|{if !name().trim().is_empty(){cx.state.write().settings.name=name().trim().to_string();}},"Save"}}
        button{class:"text-button",onclick:move |_|show_code.set(!show_code()),if show_code(){"Hide my Snapcode"}else{"Show my Snapcode"}Icon{name:"qr"}}
        if show_code(){textarea{class:"invite-text",readonly:true,value:"{invite}","aria-label":"Your n0-snap invitation"}p{class:"form-hint","Copy this invitation to a friend. They paste it in Add a friend; you accept their request here."}}
        }if show_code(){div{class:"qr-card",dangerous_inner_html:"{svg}"}}}
    }}
}

#[component]
fn AddFriend() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut input = use_signal(String::new);
    let mut found = use_signal(|| None::<PublicProfile>);
    let mut status = use_signal(String::new);
    let mut busy = use_signal(|| false);
    rsx! {div{class:"modal-backdrop",div{class:"modal",role:"dialog","aria-modal":"true","aria-label":"Add a friend",
        div{class:"modal-heading",div{p{class:"eyebrow","A FAMILIAR FACE"}h2{"Find a friend."}}button{class:"icon-button",title:"Close",onclick:move |_|cx.add.set(false),Icon{name:"close"}}}
        p{class:"muted","Look up an AT Protocol handle, or paste a n0-snap Snapcode invitation."}
        textarea{class:"field invite-input",placeholder:"alice.bsky.social or flicker://friend/…",value:"{input}",oninput:move |e|{input.set(e.value());found.set(None);},maxlength:16000}
        if let Some(profile)=found(){div{class:"lookup-result",div{class:"avatar you","{profile.name.chars().next().unwrap_or('@')}"}div{strong{if profile.name.is_empty(){"{profile.handle}"}else{"{profile.name}"}}p{"@{profile.handle}"}small{"{profile.did}"}}}p{class:"form-hint","Public profile found. Ask this person for their n0-snap Snapcode to enable messaging."}}
        if !status().is_empty(){p{class:"form-error",role:"status","{status}"}}
        div{class:"modal-footer",span{class:"form-hint","Search names and handles in Discover."}button{class:"primary",disabled:busy()||input().trim().is_empty(),onclick:move |_|{
            let value=input().trim().to_string();
            if value.starts_with("flicker://"){
                match Invite::decode(&value){Ok(inv)=>{
                    let id=inv.endpoint.id.to_string();
                    if cx.client.read().as_ref().is_some_and(|n|n.endpoint.id()==inv.endpoint.id){status.set("That's your own Snapcode.".into());return;}
                    if cx.state.read().contacts.iter().any(|c|c.id==id){status.set("Already in your friends.".into());return;}
                    let node=cx.client.read().clone();let endpoint_id=inv.endpoint.id;
                    let Some(node)=node else {status.set("iroh is still connecting".into());return;};
                    if inv.request_token.is_empty() {
                        cx.state.write().connect_friend(&inv);
                        spawn(async move {node.allowed.write().await.insert(endpoint_id);});
                        cx.add.set(false);cx.toast.set("Older Snapcode added. Have them add your code too.".into());
                        return;
                    }
                    if cx.state.read().friend_requests.iter().filter(|r|r.outgoing).count()>=32 {status.set("You have 32 pending requests. Cancel one in Friends first.".into());return;}
                    if !cx.state.read().friend_requests.iter().any(|r|r.outgoing && r.invite.endpoint.id==endpoint_id) {
                        cx.state.write().friend_requests.push(FriendRequest{invite:inv.clone(),outgoing:true,declined:false});
                    }
                    busy.set(true);status.set(String::new());
                    spawn(async move {
                        match node.request_friend(&inv,own_invite(cx,&node)).await {
                            Ok(FriendStatus::Accepted)=>{cx.state.write().connect_friend(&inv);node.allowed.write().await.insert(endpoint_id);cx.add.set(false);cx.toast.set("You're connected. Send a moment!".into());},
                            Ok(FriendStatus::Pending)=>{cx.add.set(false);cx.page.set(Page::Friends);cx.toast.set("Request sent. We'll connect when they accept.".into());},
                            Ok(FriendStatus::Declined)=>{if let Some(pending)=cx.state.write().friend_requests.iter_mut().find(|r|r.outgoing&&r.invite.endpoint.id==endpoint_id){pending.declined=true;}status.set("This friend request was declined.".into());},
                            Err(e)=>status.set(format!("{e} Your request is saved in Friends and will retry while the app is open.")),
                        }
                        busy.set(false);
                    });
                },Err(e)=>status.set(e.to_string())}
            }else if let Some(profile)=found(){
                if !cx.state.read().contacts.iter().any(|c|c.did==profile.did){cx.state.write().contacts.push(Contact{id:profile.did.clone(),did:profile.did,name:if profile.name.is_empty(){profile.handle.clone()}else{profile.name},handle:profile.handle,color:"#c3ccdc".into(),endpoint:None});}
                cx.add.set(false);cx.toast.set("Profile added. Exchange Snapcodes to send media.".into());
            }else{
                busy.set(true);status.set(String::new());spawn(async move{
                    let result:Result<PublicProfile>=async{flicker::discovery::Discovery::new()?.profile(&value).await}.await;
                    match result{Ok(p)=>found.set(Some(p)),Err(_)=>status.set("Couldn't find that profile. Check the full handle and your connection.".into())}busy.set(false);
                });
            }
        },if busy(){"Looking…"}else if found.read().is_some(){"Add profile"}else if input().starts_with("flicker://"){"Request connection"}else{"Find profile"}}}
    }}}
}

#[component]
fn Hosting() -> Element {
    let mut cx = use_context::<Ctx>();
    let settings = cx.state.read().settings.clone();
    let mut mode = use_signal(|| settings.host_mode);
    let mut endpoint = use_signal(|| settings.endpoint);
    let mut token = use_signal(|| settings.write_token);
    let mut testing = use_signal(|| false);
    let mut result = use_signal(String::new);
    let local_id = cx
        .client
        .read()
        .as_ref()
        .map(|c| c.local_store.endpoint.id().to_string())
        .unwrap_or("Starting…".into());
    rsx! {section{class:"hosting-page",
        div{class:"host-explainer",span{class:"host-illustration",Icon{name:"cloud"}}div{p{class:"eyebrow","YOU PICK THE ADDRESS"}h2{"Your moments need a home."}p{"Your host holds encrypted photos and videos so friends can open them later. Run your own, or connect a cloud endpoint. The keys stay with you and the people you share with."}}}
        div{class:"host-options",for (id,icon,title,desc) in [("local","laptop","This device","Ready now. Available while n0-snap is open."),("personal","server","My own server","An always-on endpoint, managed by you."),("cloud","cloud","Cloud endpoint","Connect a hosted n0-snap storage endpoint.")]{button{class:if mode()==id{"host-option chosen"}else{"host-option"},onclick:move |_|{mode.set(id.into());result.set(String::new());},Icon{name:icon}strong{"{title}"}p{"{desc}"}span{class:"radio-mark"}}}}
        div{class:"host-config",
            if mode()=="local"{h3{"Your local iroh endpoint"}p{class:"muted","Encrypted media is stored on this computer. Friends need this app to remain open to fetch it."}code{class:"endpoint-code","{local_id}"}}
            else{h3{if mode()=="personal"{"Connect your server"}else{"Connect a cloud host"}}p{class:"muted","Paste the endpoint ID printed by flicker-store and its write token. No cloud service has been provisioned automatically."}label{class:"field-label","Iroh endpoint ID"}input{class:"field",placeholder:"64-character endpoint public key",value:"{endpoint}",oninput:move |e|endpoint.set(e.value())}label{class:"field-label","Storage write token"}input{r#type:"password",class:"field",placeholder:"Your host's secret write token",value:"{token}",oninput:move |e|token.set(e.value())}}
            div{class:"modal-footer",span{class:"form-hint","iroh 1.3 · encrypted media · 24h maximum storage"}button{class:"primary",disabled:testing(),onclick:move |_|{
                let setting=Settings{name:cx.state.read().settings.name.clone(),handle:cx.state.read().settings.handle.clone(),host_mode:mode(),endpoint:endpoint(),write_token:token()};
                let Some(node)=cx.client.read().clone()else{result.set("iroh is still connecting".into());return;};testing.set(true);result.set(String::new());
                spawn(async move{let check:Result<()>=async{let(addr,token)=host_target(&node,&setting)?;node.probe(addr,token).await}.await;match check{Ok(())=>{cx.state.write().settings=setting;result.set("Connected. New moments will use this host.".into());},Err(e)=>result.set(e.to_string())}testing.set(false);});
            },if testing(){"Connecting…"}else{"Test & save"}Icon{name:"arrow"}}}
            if !result().is_empty(){p{class:"host-result",role:"status","{result}"}}
        }
        div{class:"hosting-facts",div{Icon{name:"lock"}strong{"Only encrypted media"}p{"Your storage host never receives the media decryption key."}}div{Icon{name:"clock"}strong{"Nothing stays by default"}p{"Host objects expire in 24 hours. Saved Memories live on your device."}}div{Icon{name:"people"}strong{"Your social identity travels"}p{"AT Protocol helps find people. Account sign-in and device attestations are coming next."}}}
    }}
}

#[component]
fn Avatar(contact: Contact) -> Element {
    let initials: String = contact
        .name
        .split_whitespace()
        .take(2)
        .filter_map(|s| s.chars().next())
        .collect();
    rsx! {div{class:"avatar",style:"background:{contact.color}","{initials}"}}
}

#[component]
fn Icon(name: String) -> Element {
    let path = match name.as_str() {
        "inbox" => "M4 4h16v16H4z M4 13h4l2 3h4l2-3h4",
        "stories" => "M12 3a9 9 0 1 1-8.1 5 M3 3v5h5 M10 8l6 4-6 4z",
        "bookmark" => "M6 3h12v18l-6-4-6 4z",
        "people" => {
            "M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2 M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8 M17 4a4 4 0 0 1 0 7 M22 21v-2a4 4 0 0 0-3-3.87"
        }
        "cloud" => "M7 18a5 5 0 1 1 .7-9.95A7 7 0 0 1 21 11a3.5 3.5 0 0 1-1.5 7H7z",
        "plus" => "M12 5v14 M5 12h14",
        "arrow" => "M5 12h14 M13 6l6 6-6 6",
        "close" => "M6 6l12 12 M6 18L18 6",
        "search" => "M21 21l-5-5 M10.5 18a7.5 7.5 0 1 0 0-15 7.5 7.5 0 0 0 0 15",
        "edit" => {
            "M12 4H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h13a2 2 0 0 0 2-2v-7 M16 3l5 5-10 10-5 1 1-5z"
        }
        "camera" => "M3 6h5l2-3h4l2 3h5v15H3z M12 17a4 4 0 1 0 0-8 4 4 0 0 0 0 8",
        "lock" => "M6 10h12v11H6z M8 10V6a4 4 0 0 1 8 0v4",
        "clock" => "M12 22a10 10 0 1 0 0-20 10 10 0 0 0 0 20 M12 6v6l4 2",
        "laptop" => "M4 3h16v13H4z M2 20h20l-2-4H4z",
        "server" => "M3 3h18v7H3z M3 14h18v7H3z M7 6.5h.01 M7 17.5h.01",
        "qr" => {
            "M3 3h6v6H3z M15 3h6v6h-6z M3 15h6v6H3z M15 15h3v3h3v3h-6z M12 3v6 M3 12h6 M12 12h9 M12 15v6"
        }
        _ => "M12 1v22 M1 12h22 M4.2 4.2l15.6 15.6 M4.2 19.8L19.8 4.2",
    };
    rsx! {svg{width:"20",height:"20",view_box:"0 0 24 24",fill:"none",stroke:"currentColor",stroke_width:"1.6",stroke_linecap:"round",stroke_linejoin:"round","aria-hidden":"true",path{d:path}}}
}
