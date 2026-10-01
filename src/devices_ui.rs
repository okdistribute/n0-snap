use super::*;
use flicker::devices::{self, Directory, VerifiedDevice};

#[component]
pub(super) fn DeviceRegistration() -> Element {
    let cx = use_context::<Ctx>();
    let mut busy = use_signal(|| false);
    let mut verified = use_signal(|| false);
    let mut message = use_signal(String::new);
    let mut checked = use_signal(|| false);
    use_effect(move || {
        let Some(node) = cx.client.read().clone() else {
            return;
        };
        let did = cx.account.read().profile.did.clone();
        spawn(async move {
            let result = async { Directory::new()?.verify(&did, node.endpoint.id()).await }.await;
            verified.set(result.is_ok());
            checked.set(true);
        });
    });
    rsx! {section {class:"device-registration",
        h3 {"Find this device through Bluesky"}
        p {class:"muted","Publish this device’s public ID in your account’s PDS. Friends can request a connection without a QR code. You still choose who to accept."}
        p {class:"form-hint","This is public, including to people you don’t follow. No private keys, access tokens, IP addresses, or snaps are published."}
        if verified() {
            p {class:"device-status",role:"status","✓ This device has a verified PDS record"}
        } else if checked() {
            p {class:"form-hint","This device is not currently verified as discoverable."}
        }
        div {class:"profile-actions",
            button {class:"primary",disabled:busy()||cx.client.read().is_none(),onclick:move |_| {
                let Some(node)=cx.client.read().clone() else {return;};
                let account=cx.account.read().clone();
                busy.set(true);message.set(String::new());
                spawn(async move {
                    let result: Result<_> = async {
                        let key = flicker::network::secret(&account_dir(&data_dir(), &account.profile.did).join("device.key"))?;
                        anyhow::ensure!(key.public()==node.endpoint.id(),"Local device key mismatch");
                        devices::publish(&account,&key).await
                    }.await;
                    match result {
                        Ok(_)=>{verified.set(true);message.set("Published and verified. Friends can now find this device from your profile.".into());},
                        Err(e)=>message.set(format!("{e:#}")),
                    }
                    busy.set(false);
                });
            },if busy(){"Working…"}else if verified(){"Verify / republish"}else{"Make this device discoverable"}}
            // Keep removal available even when the current record fails verification.
            button {class:"secondary",disabled:busy()||cx.client.read().is_none(),onclick:move |_| {
                let Some(node)=cx.client.read().clone() else {return;};
                let account=cx.account.read().clone();
                busy.set(true);message.set(String::new());
                spawn(async move {
                    match devices::revoke(&account,node.endpoint.id()).await {
                        Ok(())=>{verified.set(false);message.set("Device record removed. Account-based connections will fail their next verification. Snapcode-only connections are separate.".into());},
                        Err(e)=>message.set(format!("{e:#}")),
                    }
                    busy.set(false);
                });
            },"Remove device record"}
        }
        if !message().is_empty(){p {class:"form-hint",role:"status","{message}"}}
    }}
}

#[component]
pub(super) fn ProfileDevices(profile: PublicProfile) -> Element {
    let mut cx = use_context::<Ctx>();
    let mut devices = use_signal(Vec::<VerifiedDevice>::new);
    let mut busy = use_signal(|| false);
    let mut checked = use_signal(|| false);
    let mut message = use_signal(String::new);
    let find = profile.clone();
    rsx! {
        button {class:"primary",disabled:busy(),onclick:move |_| {
            let did=find.did.clone();
            busy.set(true);message.set("Checking signed device records…".into());devices.set(vec![]);checked.set(false);
            spawn(async move {
                match async { Directory::new()?.list(&did).await }.await {
                    Ok(found)=>{
                        message.set(if found.is_empty(){"No published n0-snap devices. You can still exchange Snapcodes.".into()}else{"Choose a device to request a connection. Both apps must be open.".into()});
                        devices.set(found);checked.set(true);
                    },
                    Err(e)=>message.set(format!("Could not verify devices: {e}. Try again or exchange Snapcodes.")),
                }
                busy.set(false);
            });
        },if busy(){"Checking…"}else if checked(){"Refresh devices"}else{"Connect via Bluesky"}}
        if !message().is_empty(){p {class:"form-hint",role:"status","{message}"}}
        for device in devices() {
            {let target=device.invite(profile.label().chars().take(80).collect(),profile.handle.clone());
            let short=device.endpoint().to_string()[..12].to_string();
            rsx! {button {class:"secondary device-choice",disabled:busy()||cx.client.read().is_none(),onclick:move |_| {
                let Some(node)=cx.client.read().clone() else {return;};
                let target=target.clone();
                if target.endpoint.id==node.endpoint.id(){message.set("This is your current device.".into());return;}
                if cx.state.read().contacts.iter().any(|c|c.endpoint.as_ref().is_some_and(|e|e.id==target.endpoint.id)){message.set("Already connected to this device.".into());return;}
                if cx.state.read().friend_requests.iter().filter(|r|r.outgoing).count()>=32{message.set("Cancel a pending request in Friends first.".into());return;}
                busy.set(true);message.set("Verifying both account records and contacting this device…".into());
                spawn(async move {
                    // Check before saving a retry, so unregistered local devices get a
                    // clear action instead of a permanently failing pending request.
                    let account=cx.account.read().profile.did.clone();
                    let verified=async {Directory::new()?.verify(&account,node.endpoint.id()).await}.await;
                    if let Err(e)=verified {message.set(format!("First make this device discoverable in Friends. {e}"));busy.set(false);return;}
                    if !cx.state.read().friend_requests.iter().any(|r|r.outgoing&&r.invite.endpoint.id==target.endpoint.id){
                        cx.state.write().friend_requests.push(FriendRequest{invite:target.clone(),outgoing:true,declined:false});
                    }
                    match node.request_friend(&target,own_invite(cx,&node)).await {
                        Ok(FriendStatus::Accepted)=>match node.authorize_friend(&target).await {
                            Ok(())=>{cx.state.write().connect_friend(&target);message.set("Connected.".into());},
                            Err(e)=>message.set(format!("Could not verify acceptance: {e}")),
                        },
                        Ok(FriendStatus::Pending)=>{message.set("Request sent. They can accept in Friends.".into());},
                        Ok(FriendStatus::Declined)=>{
                            if let Some(r)=cx.state.write().friend_requests.iter_mut().find(|r|r.outgoing&&r.invite.endpoint.id==target.endpoint.id){r.declined=true;}
                            message.set("Request declined.".into());
                        },
                        Err(e)=>message.set(format!("{e:#}. Saved in Friends; retries while this app is open.")),
                    }
                    busy.set(false);
                });
            },"Request connection · {short}"}}
            }
        }
    }
}
