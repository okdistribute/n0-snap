//! Native system UI. iroh, encryption and storage remain in Rust on every target.
use anyhow::{Result, ensure};
use std::sync::Mutex;
pub static PENDING_INVITE: Mutex<Option<String>> = Mutex::new(None);
pub type Media = (Vec<u8>, String, String);

pub fn receive_link(value: &str) {
    if flicker::model::Invite::decode(value).is_ok() {
        *PENDING_INVITE.lock().unwrap() = Some(value.to_owned());
    }
}

pub struct Browser;
impl Browser {
    pub fn open(url: &str) -> Result<Self> {
        #[cfg(target_os = "ios")]
        unsafe {
            ios::snap_browser_open(std::ffi::CString::new(url)?.as_ptr());
        }
        #[cfg(not(target_os = "ios"))]
        webbrowser::open(url)?;
        Ok(Self)
    }
}
impl Drop for Browser {
    fn drop(&mut self) {
        #[cfg(target_os = "ios")]
        unsafe {
            ios::snap_browser_close();
        }
    }
}

#[cfg(not(target_os = "ios"))]
pub async fn pick_media() -> Result<Option<Media>> {
    let Some(file) = rfd::AsyncFileDialog::new()
        .add_filter(
            "Photos & videos",
            &["jpg", "jpeg", "png", "webp", "gif", "mp4", "webm", "mov"],
        )
        .pick_file()
        .await
    else {
        return Ok(None);
    };
    ensure!(
        std::fs::metadata(file.path())?.len() <= flicker::network::MAX_MEDIA as u64,
        "Choose media smaller than 12 MB"
    );
    let ext = file
        .path()
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => anyhow::bail!("Unsupported media type"),
    };
    let bytes = file.read().await;
    ensure!(
        bytes.len() <= flicker::network::MAX_MEDIA,
        "Choose media smaller than 12 MB"
    );
    Ok(Some((bytes, mime.into(), file.file_name())))
}

#[cfg(target_os = "ios")]
pub async fn pick_media() -> Result<Option<Media>> {
    use base64::Engine;
    let Some(value) = ios::action(ios::snap_pick_media).await? else {
        return Ok(None);
    };
    #[derive(serde::Deserialize)]
    struct Picked {
        data: String,
        mime: String,
        name: String,
    }
    let value: Picked = serde_json::from_str(&value)?;
    let data = base64::engine::general_purpose::STANDARD.decode(value.data)?;
    ensure!(
        data.len() <= flicker::network::MAX_MEDIA,
        "Choose media smaller than 12 MB"
    );
    ensure!(
        flicker::network::allowed_mime(&value.mime),
        "Unsupported media type"
    );
    Ok(Some((data, value.mime, value.name)))
}

#[cfg(target_os = "ios")]
pub async fn scan_qr() -> Result<Option<String>> {
    let value = ios::action(ios::snap_scan_qr).await?;
    if let Some(value) = &value {
        flicker::model::Invite::decode(value)?;
    }
    Ok(value)
}

#[cfg(target_os = "ios")]
mod ios {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};
    type Sender = tokio::sync::oneshot::Sender<Result<Option<String>>>;
    type Callback = unsafe extern "C" fn(*mut c_void, i32, *const c_char);
    type Action = unsafe extern "C" fn(*mut c_void, Callback);
    unsafe extern "C" {
        pub fn snap_scan_qr(context: *mut c_void, callback: Callback);
        pub fn snap_pick_media(context: *mut c_void, callback: Callback);
        pub fn snap_browser_open(url: *const c_char);
        pub fn snap_browser_close();
    }
    // Native code calls exactly once on success, cancel, or error. It owns the
    // context until then; cancellation of the Rust task cannot free it early.
    unsafe extern "C" fn complete(context: *mut c_void, status: i32, text: *const c_char) {
        let sender = unsafe { Box::from_raw(context as *mut Sender) };
        let value = if text.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        };
        let result = match status {
            0 => Ok(Some(value)),
            1 => Ok(None),
            _ => Err(anyhow::anyhow!(value)),
        };
        let _ = sender.send(result);
    }
    pub async fn action(action: Action) -> Result<Option<String>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        unsafe {
            action(Box::into_raw(Box::new(tx)).cast(), complete);
        }
        rx.await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ignores_untrusted_non_invitation_links() {
        receive_link("https://example.com");
        receive_link("flicker://friend/invalid");
        assert!(PENDING_INVITE.lock().unwrap().is_none());
    }
}
