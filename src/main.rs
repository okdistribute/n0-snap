mod auth_ui;
mod devices_ui;
mod discover_ui;
mod platform;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
#[cfg(feature = "desktop")]
use dioxus::desktop as native_ui;
#[cfg(all(feature = "mobile", not(feature = "desktop")))]
use dioxus::mobile as native_ui;
use dioxus::prelude::*;
use discover_ui::Discover;
use flicker::discovery::Profile as PublicProfile;
use flicker::{
    model::*,
    network::{self, Client, FriendStatus},
};
use std::{sync::Arc, time::Duration};

fn main() {
    #[cfg(feature = "desktop")]
    let launcher = dioxus::LaunchBuilder::desktop();
    #[cfg(all(feature = "mobile", not(feature = "desktop")))]
    let launcher = dioxus::LaunchBuilder::mobile();
    let config = native_ui::Config::new()
        .with_background_color((16, 16, 18, 255))
        .with_custom_event_handler(|event, _| {
            if let native_ui::tao::event::Event::Opened { urls } = event {
                for url in urls {
                    platform::receive_link(url.as_str());
                }
            }
        });
    #[cfg(not(target_os = "ios"))]
    let config = config.with_window(
        native_ui::WindowBuilder::new()
            .with_title("n0-snap")
            .with_inner_size(native_ui::LogicalSize::new(1320.0, 860.0))
            .with_min_inner_size(native_ui::LogicalSize::new(860.0, 650.0)),
    );
    launcher.with_cfg(config).launch(auth_ui::AuthGate);
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Snaps,
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
    account: Signal<flicker::auth::Account>,
    state: Signal<State>,
    client: Signal<Option<Arc<Client>>>,
    page: Signal<Page>,
    selected: Signal<String>,
    toast: Signal<String>,
    viewer: Signal<Option<Viewer>>,
    compose: Signal<bool>,
    add: Signal<bool>,
    add_profile: Signal<Option<PublicProfile>>,
    add_input: Signal<String>,
    my_code: Signal<bool>,
    tick: Signal<u64>,
}

#[component]
fn App(account: flicker::auth::Account, initial: State) -> Element {
    let directory = use_signal(|| account_dir(&data_dir(), &account.profile.did));
    let account = use_signal(|| account);
    let mut state = use_signal(|| initial);
    let mut client = use_signal(|| None::<Arc<Client>>);
    let page = use_signal(|| {
        if state.read().contacts.iter().any(|c| c.endpoint.is_some()) {
            Page::Snaps
        } else {
            Page::Discover
        }
    });
    let selected = use_signal(String::new);
    let mut toast = use_signal(String::new);
    let mut viewer = use_signal(|| None::<Viewer>);
    let compose = use_signal(|| false);
    let add = use_signal(|| false);
    let add_profile = use_signal(|| None::<PublicProfile>);
    let add_input = use_signal(String::new);
    let my_code = use_signal(|| false);
    let mut tick = use_signal(now);
    let mut cx = Ctx {
        account,
        state,
        client,
        page,
        selected,
        toast,
        viewer,
        compose,
        add,
        add_profile,
        add_input,
        my_code,
        tick,
    };
    use_context_provider(|| cx);
    use_drop(move || {
        if let Some(node) = client.read().clone() {
            tokio::spawn(async move {
                node.endpoint.close().await;
                node.local_store.endpoint.close().await;
            });
        }
    });
    use_effect(move || {
        if let Err(e) = state.read().save(&directory.read()) {
            toast.set(format!("Could not save locally: {e}"));
        }
    });
    use_future(move || async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let (friend_tx, mut friend_rx) = tokio::sync::mpsc::channel(32);
        match Client::start_with_requests(directory(), tx, friend_tx).await {
            Ok(node) => {
                let node = Arc::new(node);
                *node.accounts.own.write().await = Some(cx.account.read().profile.did.clone());
                let contacts = state.read().contacts.clone();
                for contact in contacts {
                    if let Some(addr) = &contact.endpoint {
                        node.allowed.write().await.insert(addr.id);
                        if !contact.did.is_empty() {
                            node.accounts
                                .bindings
                                .write()
                                .await
                                .insert(addr.id, contact.did.clone());
                        }
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
                            toast.set(format!("New snap from {}", friend.name));
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
                    if let Err(e) = node.authorize_friend(&request.invite).await {
                        toast.set(format!("Could not verify connection: {e}"));
                        continue;
                    }
                    state.write().connect_friend(&request.invite);
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
            if let Some(invite) = platform::PENDING_INVITE.lock().unwrap().take() {
                cx.add_input.set(invite);
                cx.add_profile.set(None);
                cx.my_code.set(false);
                cx.add.set(true);
            }
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
        Page::Snaps => "Snaps",
        Page::Stories => "Stories",
        Page::Memories => "Memories",
        Page::Friends => "Friends",
        Page::Discover => "Discover",
        Page::Hosting => "Hosting",
    };
    rsx! {
        div { class:"app-shell",
            Sidebar {}
            main { class:"workspace",
                header { class:"topbar",
                    h1 {"{title}"}
                    div { class:"topbar-right",
                        span { class:"connection", span { class:if client.read().is_some(){"dot"}else{"dot pending"} } if client.read().is_some(){"Ready"}else{"Starting…"} }
                        button { class:"secondary", disabled:client.read().is_none(), onclick:move |_|cx.my_code.set(true), Icon{name:"qr"} "My Snapcode" }
                        button { class:"primary", disabled:client.read().is_none()||!state.read().contacts.iter().any(|c|c.endpoint.is_some()), onclick:move |_|cx.compose.set(true), Icon{name:"plus"} "New snap" }
                    }
                }
                match page() {
                    Page::Snaps | Page::Stories | Page::Memories=>rsx!{ Gallery{page:page()} },
                    Page::Friends=>rsx!{ Friends{} },
                    Page::Discover=>rsx!{ Discover{} },
                    Page::Hosting=>rsx!{ Hosting{} },
                }
            }
        }
        if !toast.read().is_empty() { div {class:"toast", role:"status", "{toast}" button {class:"icon-button", title:"Dismiss", onclick:move |_|toast.set(String::new()), Icon{name:"close"}}} }
        if compose() { Composer{} }
        if add() { AddFriend{} }
        if my_code() { MySnapcode{} }
        if viewer.read().is_some() { SnapViewer{} }
    }
}

#[component]
fn Sidebar() -> Element {
    let mut cx = use_context::<Ctx>();
    let nav = [
        (Page::Snaps, "camera", "Snaps"),
        (Page::Stories, "stories", "Stories"),
        (Page::Memories, "bookmark", "Memories"),
        (Page::Discover, "search", "Discover"),
        (Page::Friends, "people", "Friends"),
    ];
    rsx! {
        aside {class:"sidebar",
            a {class:"wordmark", href:"#", onclick:move |_|cx.page.set(Page::Snaps), "n0-snap" }
            nav { for (page,icon,label) in nav { button {class:if (cx.page)()==page{"nav-item active"}else{"nav-item"},onclick:move |_|cx.page.set(page),Icon{name:icon} "{label}" if page==Page::Friends { {let count=cx.state.read().friend_requests.iter().filter(|r|!r.outgoing).count();rsx!{if count>0{span{class:"nav-count","{count}"}}}} } } } }
            div {class:"sidebar-bottom",
                button {class:if (cx.page)()==Page::Hosting{"nav-item active"}else{"nav-item"},onclick:move |_|cx.page.set(Page::Hosting),Icon{name:"cloud"} "Hosting"}
                div {class:"local-profile",strong {"{cx.account.read().profile.label()}"} p {class:"account-handle","@{cx.account.read().profile.handle}"} auth_ui::SignOut {} }
            }
        }
    }
}

#[component]
fn Gallery(page: Page) -> Element {
    let mut cx = use_context::<Ctx>();
    let memories = page == Page::Memories;
    let mut seen = std::collections::HashSet::new();
    let items: Vec<_> = cx
        .state
        .read()
        .items
        .iter()
        .filter(|i| match page {
            Page::Memories => i.saved,
            Page::Stories => i.kind == "story" && !i.expired((cx.tick)()),
            Page::Snaps => i.kind == "snap" && !i.expired((cx.tick)()),
            _ => false,
        })
        .filter(|i| {
            seen.insert(
                i.ticket
                    .as_ref()
                    .map(|t| t.id.clone())
                    .unwrap_or(i.id.clone()),
            )
        })
        .rev()
        .cloned()
        .collect();
    rsx! {section {class:"gallery-section",
        p {class:"muted",match page {Page::Memories=>"Saved on this device",Page::Stories=>"Available for 24 hours",_=>"Snaps can be viewed for 60 seconds after opening."}}
        if items.is_empty(){div {class:"large-empty",h2 {match page {Page::Memories=>"No saved snaps",Page::Stories=>"No stories",_=>"No snaps"}}p {if memories{"Choose Save to Memories while viewing a snap."}else{"Use New snap to share a photo or video."}}}}
        div {class:"gallery-grid",for item in items {
            {let contact=cx.state.read().contacts.iter().find(|c|c.id==item.contact).cloned();let name=if item.outgoing{"You".into()}else{contact.map(|c|c.name).unwrap_or("Friend".into())};let img=item.sample.as_deref().map(sample_uri);let open=item.clone();let remove=item.id.clone();
            rsx!{article {class:"gallery-card",key:"{item.id}",button {class:"gallery-cover",onclick:move |_|open_item(cx,open.clone(),memories),if let Some(img)=img{img{src:"{img}",alt:""}}else{div {class:"media-placeholder",Icon{name:"camera"}}}span{class:"gallery-shade"}span{class:"gallery-overline",if item.sample.is_some(){"Sample · "}if memories{"Saved"}else if item.outgoing{"Sent · {relative(item.created_at)}"}else{"{relative(item.created_at)}"}}span{class:"gallery-title",if item.caption.is_empty(){"Open snap"}else{"{item.caption}"}}span{class:"gallery-name","{name}"}}if memories {button {class:"delete-memory",onclick:move |_|{if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==remove){item.saved=false;item.memory_cipher=None;}cx.toast.set("Removed from Memories".into());},Icon{name:"close"}"Remove"}}}}
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
        cx.toast.set("This snap has expired.".into());
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
                .context("This snap is no longer available")?;
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
                    cx.toast.set("This snap has expired.".into());
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
        div {class:"viewer-top",span{class:"viewer-logo","n0-snap"}span{class:"viewer-timer",Icon{name:"clock"}if let Some(s)=remaining{"{s}s"}else{"In your Memories"}}button {class:"viewer-close",title:"Close snap",onclick:move |_|{if view.item.kind!="story"&&view.deadline.is_some(){if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==view.item.id){item.opened_at=Some(now().saturating_sub(60));}}cx.viewer.set(None);},Icon{name:"close"}}}
        if let Some(s)=remaining {div {class:"viewer-progress",div {style:"width:{s as f64 / 60.0 * 100.0}%"}}}
        div {class:"viewer-media",
            if view.mime.starts_with("video/"){video {src:"{view.uri}",autoplay:true,controls:true,playsinline:true}}
            else if view.mime=="text/plain"{div {class:"text-snap","{view.uri}"}}
            else {img {src:"{view.uri}",alt:"Snap content"}}
            div {class:"viewer-caption","{view.item.caption}"}
        }
        div {class:"viewer-bottom",p {if saved{"Kept on this device."}else{"Save a copy on this device."}}button {class:"save-memory",disabled:saved,onclick:move |_|{if let Some(item)=cx.state.write().items.iter_mut().find(|i|i.id==for_save.item.id){item.saved=true;item.memory_cipher=for_save.cipher.clone();}cx.toast.set("Saved to Memories on this device".into());},Icon{name:"bookmark"}if saved{"Saved to Memories"}else{"Save to Memories"}}}
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
    ensure!(!contacts.is_empty(), "Connect a friend before sending");
    ensure!(
        contacts.iter().all(|c| c.endpoint.is_some()),
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
            "Snap sent.".into()
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
            "Add an endpoint in Hosting first"
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
    let mut kind = use_signal(|| {
        if (cx.page)() == Page::Stories {
            "story"
        } else {
            "snap"
        }
        .to_string()
    });
    let mut recipient = use_signal(|| {
        let state = cx.state.read();
        state
            .contacts
            .iter()
            .find(|c| c.id == (cx.selected)() && c.endpoint.is_some())
            .or_else(|| state.contacts.iter().find(|c| c.endpoint.is_some()))
            .map(|c| c.id.clone())
            .unwrap_or_default()
    });
    let mut busy = use_signal(|| false);
    let mut error = use_signal(String::new);
    let contacts: Vec<_> = cx
        .state
        .read()
        .contacts
        .iter()
        .filter(|c| c.endpoint.is_some())
        .cloned()
        .collect();
    let preview = chosen
        .read()
        .as_ref()
        .map(|(b, m, _)| format!("data:{m};base64,{}", STANDARD.encode(b)));
    rsx! {div {class:"modal-backdrop",div {class:"modal composer",role:"dialog","aria-modal":"true","aria-label":"New snap",
        div {class:"modal-heading",h2{"New snap"}button{class:"icon-button",title:"Close",disabled:busy(),onclick:move |_|cx.compose.set(false),Icon{name:"close"}}}
        div {class:"compose-tabs",button{class:if kind()=="snap"{"active"}else{""},onclick:move |_|kind.set("snap".into()),"Direct snap"}button{class:if kind()=="story"{"active"}else{""},onclick:move |_|kind.set("story".into()),"24-hour story"}}
        button {class:"upload-area",disabled:busy(),onclick:move |_|{busy.set(true);spawn(async move{
            match platform::pick_media().await {Ok(Some(media))=>{chosen.set(Some(media));error.set(String::new());},Ok(None)=>{},Err(e)=>error.set(e.to_string())}
            busy.set(false);
        });},
            if let Some(uri)=preview {if chosen.read().as_ref().is_some_and(|(_,m,_)|m.starts_with("video")){video{src:"{uri}",muted:true}}else{img{src:"{uri}",alt:"Your selected photo"}}span{class:"replace-media","Choose something else"}}
            else {span{class:"upload-icon",Icon{name:"camera"}}strong{"Choose a photo or video"}p{"Choose from your device · up to 12 MB"}}
        }
        label {class:"field-label","Caption (optional)"}input{class:"field",placeholder:"Add a caption",value:"{caption}",maxlength:2000,oninput:move |e|caption.set(e.value())}
        if kind()=="snap"{label{class:"field-label","For"}select{class:"field",value:"{recipient}",onchange:move |e|recipient.set(e.value()),for contact in contacts{option{value:"{contact.id}","{contact.name}"}}}}else{p{class:"form-hint","Shared with all added friends. Real peers must be online to receive the story invitation; media stays on your host."}}
        if !error().is_empty(){p{class:"form-error",role:"alert","{error}"}}
        div{class:"modal-footer",span{class:"form-hint",if kind()=="snap"{"60 seconds to view. Can be saved."}else{"Visible for 24 hours."}}button{class:"primary",disabled:chosen.read().is_none()||busy(),onclick:move |_|{
            let Some((bytes,mime,_))=chosen().clone()else{return;};let contacts:Vec<_>=cx.state.read().contacts.iter().filter(|c|if kind()=="story"{c.endpoint.is_some()}else{c.id==recipient()}).cloned().collect();
            if contacts.is_empty(){error.set("Add a friend first".into());return;}busy.set(true);error.set(String::new());let caption=caption();let kind=kind();
            let destination=if kind=="story"{Page::Stories}else{Page::Snaps};
            spawn(async move{match publish(cx,bytes,mime,caption,kind,contacts).await{Ok(())=>{cx.compose.set(false);cx.page.set(destination);},Err(e)=>error.set(e.to_string())}busy.set(false);});
        },if busy(){"Sending…"}else{"Send"}Icon{name:"arrow"}}}
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
                div { class:"section-row",h2 {"Connection requests"} }
                for request in requests {
                    { let invite=request.invite.clone();let accept=invite.clone();let remove=invite.clone();
                    rsx! {div {class:"request-row",key:"{invite.endpoint.id}-{request.outgoing}",
                        div {class:"avatar you",Icon{name:"people"}}
                        div {class:"request-copy",strong {"{invite.name}"}p {class:"muted",if request.declined {"Request declined"}else if request.outgoing {"Waiting for acceptance · retries while this app is open"}else if invite.version==2 {"@{invite.handle} · verified account device"}else{"Wants to connect · device identity only"}}small {"Device {invite.endpoint.id}"}}
                        if !request.outgoing {button {class:"primary",disabled:cx.client.read().is_none(),onclick:move |_| {
                            let Some(node)=cx.client.read().clone() else {return;};
                            let id=accept.endpoint.id;
                            let accept = accept.clone();
                            spawn(async move {
                                match node.authorize_friend(&accept).await {
                                    Ok(()) => {
                                        cx.state.write().connect_friend(&accept);
                                        cx.state.write().declined_requests.retain(|e|*e!=id);
                                        node.declined.write().await.remove(&id);
                                        cx.toast.set("Accepted. Their app will connect automatically on its next check.".into());
                                    }
                                    Err(e) => cx.toast.set(format!("Device verification failed: {e}")),
                                }
                            });
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
    let friends: Vec<_> = cx
        .state
        .read()
        .contacts
        .iter()
        .filter(|c| c.endpoint.is_some())
        .cloned()
        .collect();
    rsx! {section {class:"friends-page",
        devices_ui::DeviceRegistration {}
        FriendRequests {}
        div{class:"friend-intro",div{h2{"Your friends"}p{class:"muted","Browse Discover for new people, or use a Snapcode to request a connection."}}button{class:"primary",onclick:move |_|{cx.add_input.set(String::new());cx.add_profile.set(None);cx.add.set(true);},Icon{name:"qr"}"Add Snapcode"}}
        div{class:"friend-grid",for contact in friends{div{class:"friend-card",Avatar{contact:contact.clone()}h3{"{contact.name}"}p{if contact.handle.is_empty(){"Connected device"}else{"@{contact.handle}"}}span{class:"chip","Connected device"}button{class:"text-button",onclick:move |_|{cx.selected.set(contact.id.clone());cx.compose.set(true);},"Send snap" Icon{name:"arrow"}}}}}
        div{class:"identity-card",div{
            label{class:"field-label","Your name on this device"}div{class:"inline-field",input{class:"field",value:"{name}",oninput:move |e|name.set(e.value()),maxlength:80}button{class:"secondary",onclick:move |_|{if !name().trim().is_empty(){cx.state.write().settings.name=name().trim().to_string();}},"Save"}}
        }}
        div{class:"mobile-account",p{"@{cx.account.read().profile.handle}"}button{class:"secondary",onclick:move |_|cx.page.set(Page::Hosting),"Hosting"}auth_ui::SignOut{}}
    }}
}

#[component]
fn MySnapcode() -> Element {
    let mut cx = use_context::<Ctx>();
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
    let svg = if !invite.is_empty() {
        qrcode::QrCode::new(invite.as_bytes())
            .ok()
            .map(|q| {
                q.render::<qrcode::render::svg::Color>()
                    .min_dimensions(384, 384)
                    .dark_color(qrcode::render::svg::Color("#18181b"))
                    .light_color(qrcode::render::svg::Color("#ffffff"))
                    .build()
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    rsx! {div {class:"modal-backdrop",div{class:"modal snapcode-modal",role:"dialog","aria-modal":"true","aria-label":"My Snapcode",
        div{class:"modal-heading",h2{"My Snapcode"}button{class:"icon-button",title:"Close",onclick:move |_|cx.my_code.set(false),Icon{name:"close"}}}
        p{class:"muted","@{cx.account.read().profile.handle}"}
        devices_ui::DeviceRegistration {}
        if !svg.is_empty(){div{class:"qr-card own-snapcode",dangerous_inner_html:"{svg}"}}else{p{role:"status","Your Snapcode isn't available yet."}}
        p{"Share this invitation with a friend. They add it in n0-snap; you accept their request in Friends."}
        textarea{class:"invite-text",readonly:true,value:"{invite}","aria-label":"Your n0-snap invitation"}
        p{class:"form-hint","On iPhone, open n0-snap → Add Snapcode → Scan QR code. Keep both apps open while connecting."}
    }}}
}

#[component]
fn AddFriend() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut input = cx.add_input;
    let mut status = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let prompt = cx
        .add_profile
        .read()
        .as_ref()
        .map(|profile| format!("Paste @{}’s n0-snap Snapcode invitation.", profile.handle))
        .unwrap_or_else(|| "Paste your friend's n0-snap Snapcode invitation.".into());
    rsx! {div{class:"modal-backdrop",div{class:"modal",role:"dialog","aria-modal":"true","aria-label":"Add Snapcode",
        div{class:"modal-heading",h2{"Add Snapcode"}button{class:"icon-button",title:"Close",disabled:busy(),onclick:move |_|cx.add.set(false),Icon{name:"close"}}}
        p{class:"muted","{prompt}"}
        ScanCode { input, status, busy }
        textarea{class:"field invite-input","aria-label":"Snapcode invitation",placeholder:"flicker://friend/…",value:"{input}",disabled:busy(),oninput:move |e|{input.set(e.value());status.set(String::new());},maxlength:16000}
        if !status().is_empty(){p{class:"form-error",role:"status","{status}"}}
        div{class:"modal-footer",span{class:"form-hint","They'll need to accept your request."}button{class:"primary",disabled:busy()||input().trim().is_empty(),onclick:move |_|{
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
                            Ok(FriendStatus::Accepted)=>{cx.state.write().connect_friend(&inv);node.allowed.write().await.insert(endpoint_id);cx.add.set(false);cx.toast.set("Connected.".into());},
                            Ok(FriendStatus::Pending)=>{cx.add.set(false);cx.page.set(Page::Friends);cx.toast.set("Request sent. We'll connect when they accept.".into());},
                            Ok(FriendStatus::Declined)=>{if let Some(pending)=cx.state.write().friend_requests.iter_mut().find(|r|r.outgoing&&r.invite.endpoint.id==endpoint_id){pending.declined=true;}status.set("This friend request was declined.".into());},
                            Err(e)=>status.set(format!("{e} Your request is saved in Friends and will retry while the app is open.")),
                        }
                        busy.set(false);
                    });
                },Err(e)=>status.set(e.to_string())}
            }else{
                status.set("Paste a Snapcode invitation starting with flicker://friend/.".into());
            }
        },if busy(){"Connecting…"}else{"Request connection"}}}
    }}}
}

#[component]
fn ScanCode(
    mut input: Signal<String>,
    mut status: Signal<String>,
    mut busy: Signal<bool>,
) -> Element {
    #[cfg(target_os = "ios")]
    return rsx! {button{class:"secondary scan-code",disabled:busy(),onclick:move |_|{
        busy.set(true);status.set(String::new());
        spawn(async move {
            match platform::scan_qr().await {Ok(Some(code))=>input.set(code),Ok(None)=>{},Err(e)=>status.set(e.to_string())}
            busy.set(false);
        });
    },Icon{name:"camera"}"Scan QR code"}};
    #[cfg(not(target_os = "ios"))]
    {
        let _ = (input, status, busy);
        rsx! {}
    }
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
        p{class:"muted","Choose where encrypted media is stored. Files expire after 24 hours; saved Memories stay on this device."}
        div{class:"host-options",for (id,icon,title,desc) in [("local","laptop","This device","Ready now. Available while n0-snap is open."),("personal","server","My own server","An always-on endpoint, managed by you."),("cloud","cloud","Cloud endpoint","Connect a hosted n0-snap storage endpoint.")]{button{class:if mode()==id{"host-option chosen"}else{"host-option"},onclick:move |_|{mode.set(id.into());result.set(String::new());},Icon{name:icon}strong{"{title}"}p{"{desc}"}span{class:"radio-mark"}}}}
        div{class:"host-config",
            if mode()=="local"{h3{"Your local iroh endpoint"}p{class:"muted","Encrypted media is stored on this computer. Friends need this app to remain open to fetch it."}code{class:"endpoint-code","{local_id}"}}
            else{h3{if mode()=="personal"{"Connect your server"}else{"Connect a cloud host"}}p{class:"muted","Paste the endpoint ID printed by flicker-store and its write token. No cloud service has been provisioned automatically."}label{class:"field-label","Iroh endpoint ID"}input{class:"field",placeholder:"64-character endpoint public key",value:"{endpoint}",oninput:move |e|endpoint.set(e.value())}label{class:"field-label","Storage write token"}input{r#type:"password",class:"field",placeholder:"Your host's secret write token",value:"{token}",oninput:move |e|token.set(e.value())}}
            div{class:"modal-footer",span{class:"form-hint","Test the connection before saving."}button{class:"primary",disabled:testing(),onclick:move |_|{
                let setting=Settings{name:cx.state.read().settings.name.clone(),handle:cx.state.read().settings.handle.clone(),host_mode:mode(),endpoint:endpoint(),write_token:token()};
                let Some(node)=cx.client.read().clone()else{result.set("iroh is still connecting".into());return;};testing.set(true);result.set(String::new());
                spawn(async move{let check:Result<()>=async{let(addr,token)=host_target(&node,&setting)?;node.probe(addr,token).await}.await;match check{Ok(())=>{cx.state.write().settings=setting;result.set("Connected. New snaps will use this host.".into());},Err(e)=>result.set(e.to_string())}testing.set(false);});
            },if testing(){"Connecting…"}else{"Test & save"}Icon{name:"arrow"}}}
            if !result().is_empty(){p{class:"host-result",role:"status","{result}"}}
        }
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
