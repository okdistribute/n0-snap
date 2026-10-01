use super::*;
use dioxus::dioxus_core::Task;
use flicker::auth::{self, Account};

#[derive(Clone, Copy)]
struct LoginContext {
    signed_in: Signal<Option<(Account, State)>>,
    error: Signal<String>,
    signing_out: Signal<bool>,
}

fn prepare(account: Account) -> Result<(Account, State)> {
    let mut state = State::load(&account_dir(&data_dir(), &account.profile.did)).context(
        "Could not load this account's local data. Existing data has been left in place.",
    )?;
    state.settings.name = account.profile.label().to_string();
    state.settings.handle = account.profile.handle.clone();
    state.discovery_actor = account.profile.did.clone();
    Ok((account, state))
}

#[component]
pub(super) fn AuthGate() -> Element {
    let mut signed_in = use_signal(|| None::<(Account, State)>);
    let mut error = use_signal(String::new);
    let mut restoring = use_signal(|| true);
    let signing_out = use_signal(|| false);
    use_context_provider(|| LoginContext {
        signed_in,
        error,
        signing_out,
    });
    use_future(move || async move {
        match auth::restore(&data_dir()).await {
            Ok(Some(account)) => match prepare(account) {
                Ok(ready) => signed_in.set(Some(ready)),
                Err(e) => error.set(e.to_string()),
            },
            Ok(None) => {}
            Err(e) => error.set(e.to_string()),
        }
        restoring.set(false);
    });
    rsx! {
        document::Meta {name:"viewport",content:"width=device-width, initial-scale=1, viewport-fit=cover"}
        style { {include_str!("../assets/style.css")} }
        if restoring() || signing_out() {
            main { class:"login-screen", div { class:"login-card", h1 {"n0-snap"} p {class:"muted",role:"status",if signing_out(){"Signing out…"}else{"Restoring your Bluesky session…"}} } }
        } else if let Some((account, initial)) = signed_in() {
            App { key:"{account.profile.did}", account, initial }
        } else {
            SignIn {}
        }
    }
}

#[component]
fn SignIn() -> Element {
    let mut login = use_context::<LoginContext>();
    let mut handle = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut browser_url = use_signal(String::new);
    let mut task = use_signal(|| None::<Task>);
    rsx! {
        main {class:"login-screen",
            form {class:"login-card",onsubmit:move |event| {
                event.prevent_default();
                if busy() || handle().trim().is_empty() { return; }
                busy.set(true); login.error.set(String::new()); browser_url.set(String::new());
                let handle = handle();
                let pending = spawn(async move {
                    let result: Result<(Account, State)> = async {
                        let pending = auth::begin(&data_dir(), &handle).await?;
                        browser_url.set(pending.url.clone());
                        let _browser = platform::Browser::open(&pending.url)?;
                        prepare(pending.finish().await?)
                    }.await;
                    busy.set(false); browser_url.set(String::new());
                    match result { Ok(ready) => login.signed_in.set(Some(ready)), Err(e) => login.error.set(e.to_string()) }
                });
                task.set(Some(pending));
            },
                h1 {"n0-snap"}
                h2 {"Connect your Bluesky account"}
                p {class:"muted","Sign in to find people you follow and share photos and videos with friends using n0-snap."}
                label {class:"field-label",r#for:"login-handle","Bluesky handle"}
                input {id:"login-handle",class:"field",placeholder:"you.bsky.social",autocomplete:"username",autocapitalize:"none",spellcheck:false,autofocus:true,disabled:busy(),value:"{handle}",maxlength:253,oninput:move |e|handle.set(e.value())}
                button {class:"primary login-submit",r#type:"submit",disabled:busy()||handle().trim().is_empty(),if busy(){"Waiting for Bluesky…"}else{"Continue with Bluesky"}}
                if busy() {
                    p {class:"form-hint",role:"status",if browser_url().is_empty(){"Connecting to your account's sign-in server…"}else{"Complete sign-in in your browser, then return here."}}
                    div {class:"login-actions",
                        if !cfg!(target_os="ios") && !browser_url().is_empty() {button {class:"text-button",r#type:"button",onclick:move |_|{let _=webbrowser::open(&browser_url());},"Open browser again"}}
                        button {class:"text-button",r#type:"button",onclick:move |_|{if let Some(task)=task.take(){task.cancel();}busy.set(false);browser_url.set(String::new());},"Cancel"}
                    }
                }
                if !login.error.read().is_empty() {p {class:"form-error",role:"alert","{login.error}"}}
                p {class:"form-hint","Your password stays in Bluesky. n0-snap requests access to your profile and public follow lists."}
            }
        }
    }
}

#[component]
pub(super) fn SignOut() -> Element {
    let cx = use_context::<Ctx>();
    let mut login = use_context::<LoginContext>();
    rsx! {button {class:"text-button",onclick:move |_| {
        let node = cx.client.read().clone();
        login.signing_out.set(true);
        login.signed_in.set(None);
        // Unmount account-scoped UI first to cancel outstanding profile requests.
        // Keep logout in the root scope so it survives the unmount.
        dioxus::dioxus_core::spawn_forever(async move {
            if let Some(node) = node {
                node.endpoint.close().await;
                node.local_store.endpoint.close().await;
            }
            let result = auth::logout(&data_dir()).await;
            login.error.set(result.err().map(|e|e.to_string()).unwrap_or_default());
            login.signing_out.set(false);
        });
    },"Sign out"}}
}
