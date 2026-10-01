use super::*;
use flicker::discovery::{Discovery, Suggestion};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Search,
    Following,
    Connections,
}
#[derive(Clone, Copy)]
struct Browse {
    mode: Signal<Mode>,
    query: Signal<String>,
    actor: Signal<String>,
    results: Signal<Vec<Suggestion>>,
    cursor: Signal<Option<String>>,
    busy: Signal<bool>,
    error: Signal<String>,
    note: Signal<String>,
    generation: Signal<u64>,
    loaded_query: Signal<String>,
}
fn load(mut cx: Ctx, mut b: Browse, more: bool) {
    let mode = (b.mode)();
    let query = if more {
        (b.loaded_query)()
    } else if mode == Mode::Search {
        (b.query)().trim().to_string()
    } else {
        (b.actor)().trim().trim_start_matches('@').to_string()
    };
    let generation = (b.generation)() + 1;
    b.generation.set(generation);
    if !more {
        b.results.set(vec![]);
        b.cursor.set(None);
        b.note.set(String::new());
    }
    b.error.set(String::new());
    if query.is_empty() {
        b.busy.set(false);
        return;
    }
    b.busy.set(true);
    b.loaded_query.set(query.clone());
    let cursor = if more { (b.cursor)() } else { None };
    spawn(async move {
        let result: Result<(Vec<Suggestion>, Option<String>, String)> = async {
            let api = Discovery::authenticated(cx.account.read().session.clone())?;
            match mode {
                Mode::Search => {
                    let page = api.search(&query, cursor.as_deref()).await?;
                    Ok((
                        page.actors
                            .into_iter()
                            .map(|profile| Suggestion {
                                profile,
                                via: vec![],
                            })
                            .collect(),
                        page.cursor,
                        format!("Results for “{query}”"),
                    ))
                }
                Mode::Following => {
                    let page = api.follows(&query, cursor.as_deref()).await?;
                    let note = format!("People @{} follows", page.subject.handle);
                    Ok((
                        page.follows
                            .into_iter()
                            .map(|profile| Suggestion {
                                profile,
                                via: vec![],
                            })
                            .collect(),
                        page.cursor,
                        note,
                    ))
                }
                Mode::Connections => {
                    let (people, failed) = api.connections(&query).await?;
                    let note = format!(
                        "Connections from a sample of up to 6 accounts @{query} follows. {}",
                        if failed > 0 {
                            format!("{failed} accounts couldn't be loaded.")
                        } else {
                            "Ranked by shared follows in this sample.".into()
                        }
                    );
                    Ok((people, None, note))
                }
            }
        }
        .await;
        if (b.generation)() != generation {
            return;
        }
        b.busy.set(false);
        match result {
            Ok((results, cursor, note)) => {
                if more {
                    let mut existing = b.results.write();
                    for entry in results {
                        if !existing.iter().any(|p| p.profile.did == entry.profile.did) {
                            existing.push(entry);
                        }
                    }
                } else {
                    b.results.set(results);
                }
                b.cursor.set(cursor);
                b.note.set(note);
                if matches!(mode, Mode::Following | Mode::Connections) {
                    cx.state.write().discovery_actor = query;
                }
            }
            Err(e) => b.error.set(e.to_string()),
        }
    });
}

#[component]
pub(super) fn Discover() -> Element {
    let mut cx = use_context::<Ctx>();
    let mut b = Browse {
        mode: use_signal(|| Mode::Following),
        query: use_signal(String::new),
        actor: use_signal(|| cx.account.read().profile.handle.clone()),
        results: use_signal(Vec::new),
        cursor: use_signal(|| None),
        busy: use_signal(|| false),
        error: use_signal(String::new),
        note: use_signal(String::new),
        generation: use_signal(|| 0),
        loaded_query: use_signal(String::new),
    };
    use_future(move || async move {
        load(cx, b, false);
    });
    let mode = (b.mode)();
    let results = (b.results)();
    rsx! {
        section {class:"discover-page",
            div {class:"discover-banner",
                p {class:"muted","Find people you follow. Connect through their verified PDS device records, or exchange a Snapcode."}
                button {class:"secondary",onclick:move |_|{cx.add_input.set(String::new());cx.add_profile.set(None);cx.add.set(true);},Icon{name:"qr"}"Add Snapcode"}
            }
            div {class:"discover-tabs",role:"tablist","aria-label":"Discover people",
                for (tab,label) in [(Mode::Search,"Search people"),(Mode::Following,"Following"),(Mode::Connections,"Follow connections")] {
                    button {role:"tab","aria-selected":mode==tab,class:if mode==tab{"active"}else{""},onclick:move |_|{
                        b.mode.set(tab);b.error.set(String::new());
                        load(cx,b,false);
                    },"{label}"}
                }
            }
            if mode==Mode::Search {
                form {class:"discover-search",onsubmit:move |event|{event.prevent_default();load(cx,b,false);},
                    Icon{name:"search"}
                    input {"aria-label":"Search people by name or handle",placeholder:"Name or handle",value:"{b.query}",maxlength:256,oninput:move|event|b.query.set(event.value())}
                    button {class:"primary",r#type:"submit",disabled:(b.busy)()||b.query.read().trim().chars().count()<2,"Find people" Icon{name:"arrow"}}
                }
            } else if matches!(mode,Mode::Following|Mode::Connections) {
                form {class:"network-picker",onsubmit:move |event|{event.prevent_default();load(cx,b,false);},
                    label {r#for:"network-handle","Bluesky handle"}
                    div {class:"inline-field",input {id:"network-handle",class:"field",placeholder:"you.bsky.social",value:"{b.actor}",maxlength:256,oninput:move|event|b.actor.set(event.value())}button {class:"primary",r#type:"submit",disabled:(b.busy)()||b.actor.read().trim().is_empty(),"Explore network"}}
                    p {class:"form-hint","Showing public follows. Following someone on Bluesky doesn't connect them in n0-snap."}
                }
            }
            div {class:"discover-meta",p {class:"muted",if !(b.note)().is_empty(){"{b.note}"}else{"Public Bluesky profiles · device records checked when you connect"}}}
            if !(b.error)().is_empty(){div {class:"form-error",role:"alert","{b.error}" button{class:"text-button",onclick:move |_|load(cx,b,false),"Try again"}}}
            if (b.busy)(){div{class:"discovery-loading",role:"status",span{class:"dot pending"}"Loading profiles…"}}
            if results.is_empty() && !(b.busy)() && (b.error)().is_empty() {
                div {class:"discovery-empty",h2 {if !(b.note)().is_empty(){"No profiles found"}else{"Find profiles"}}p {class:"muted",if mode==Mode::Search{"Search by name or handle. You don't need an exact match."}else{"Enter a handle above to explore its public connections."}}}
            }
            div {class:"discovery-grid",for entry in results {ProfileCard{key:"{entry.profile.did}",entry}}}
            if b.cursor.read().is_some(){div{class:"discover-more",button{class:"secondary",disabled:(b.busy)(),onclick:move |_|load(cx,b,true),"Load more people"}}}
        }
    }
}

#[component]
fn ProfileCard(entry: Suggestion) -> Element {
    let mut cx = use_context::<Ctx>();
    let profile = entry.profile;
    let mut broken_image = use_signal(|| false);
    let avatar = profile
        .avatar
        .as_ref()
        .filter(|url| url.starts_with("https://"));
    let label = profile.label().to_string();
    let initials: String = label
        .split_whitespace()
        .take(2)
        .filter_map(|s| s.chars().next())
        .collect();
    let via = entry.via.join(", ");
    let connect = profile.clone();
    rsx! {article {class:"discovery-card",
        div {class:"discovery-card-top",div {class:"discover-avatar",
            if let Some(url)=avatar {if !broken_image(){img{src:"{url}",alt:"",loading:"lazy",referrerpolicy:"no-referrer",onerror:move |_|broken_image.set(true)}}else{"{initials}"}}else{"{initials}"}
        }}
        h3 {"{label}"}p {class:"discover-handle","@{profile.handle}"}
        if !profile.description.is_empty(){p {class:"discover-bio","{profile.description}"}}
        if !entry.via.is_empty(){p{class:"discover-via","Followed by {via}"}}
        div {class:"profile-actions",
            devices_ui::ProfileDevices {profile:profile.clone()}
            button {class:"secondary",onclick:move |_|{cx.add_input.set(String::new());cx.add_profile.set(Some(connect.clone()));cx.add.set(true);},Icon{name:"qr"}"Add Snapcode"}
        }
    }}
}
