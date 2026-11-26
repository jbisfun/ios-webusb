//! Apple ID sign-in, signing and install, all in the page: isideload (wasm feature)
//! on top of this crate's WebUSB device.
//!
//! isideload talks to Apple and the anisette server with reqwest, which on wasm is
//! the page's `fetch`. Apple sends no CORS headers, so the page has to route those
//! requests itself (ferry-webinstaller replaces `fetch` for cross-origin URLs with
//! libcurl.js over a Wisp relay; TLS stays in the page).
//!
//! ```js
//! const account = await AppleAccount.login(email, password, {
//!   anisetteUrl: "https://ani.sidestore.io",
//!   twoFactor: async (params) => ({ SubmitCode: await ask(params) }),  // or "SendToDevices" (new code), null (abort)
//! });
//! await account.install(device, ipaBytes, (fraction) => {});
//! ```

use std::path::PathBuf;

use isideload::anisette::AnisetteClientInfo;
use isideload::anisette::remote_v3::RemoteV3AnisetteProvider;
use isideload::auth::apple_account::{AppleAccount as Account, TwoFactorCallbackParams, TwoFactorCallbackResponse};
use isideload::dev::developer_session::DeveloperSession;
use isideload::sideload::builder::{ExtensionsBehavior, ExtensionsBehaviorSelection, MaxCertsBehavior};
use isideload::sideload::sideloader::Sideloader;
use isideload::sideload::{SideloaderBuilder, TeamSelection};
use isideload::util::callbacks::{ExtensionsBehaviorCallbackBox, MaxCertsCallbackBox};
use isideload::util::storage::SideloadingStorage;
use rootcause::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::{IosDevice, js_error};

const DEFAULT_ANISETTE: &str = "https://ani.sidestore.io";
const MACHINE_NAME: &str = "ferry-webinstaller";

/// isideload's state (anisette provisioning, the development certificate and its
/// key, the chosen team) in `localStorage`, under `isideload:<key>`.
struct LocalStorage;

impl LocalStorage {
    fn storage() -> Result<JsValue, Report> {
        js_sys::Reflect::get(&js_sys::global(), &"localStorage".into())
            .ok()
            .filter(|s| !s.is_undefined() && !s.is_null())
            .ok_or_else(|| report!("localStorage is not available"))
    }

    fn call(name: &str, args: &[JsValue]) -> Result<JsValue, Report> {
        let storage = Self::storage()?;
        let function: js_sys::Function = js_sys::Reflect::get(&storage, &name.into())
            .map_err(|e| report!("localStorage.{name}: {e:?}"))?
            .into();
        let args: js_sys::Array = args.iter().collect();
        function.apply(&storage, &args).map_err(|e| report!("localStorage.{name}: {e:?}"))
    }
}

impl SideloadingStorage for LocalStorage {
    fn store(&self, key: &str, value: &str) -> Result<(), Report> {
        Self::call("setItem", &[format!("isideload:{key}").into(), value.into()]).map(drop)
    }

    fn retrieve(&self, key: &str) -> Result<Option<String>, Report> {
        Ok(Self::call("getItem", &[format!("isideload:{key}").into()])?.as_string().filter(|v| !v.is_empty()))
    }

    fn delete(&self, key: &str) -> Result<(), Report> {
        Self::call("removeItem", &[format!("isideload:{key}").into()]).map(drop)
    }
}

thread_local! {
    static TWO_FACTOR: std::cell::RefCell<Option<js_sys::Function>> = const { std::cell::RefCell::new(None) };
}

/// Hands the 2FA state to the page's callback and waits for its answer.
async fn ask_two_factor(params: TwoFactorCallbackParams) -> Result<TwoFactorCallbackResponse, Report> {
    let callback = TWO_FACTOR
        .with(|c| c.borrow().clone())
        .ok_or_else(|| report!("Apple asked for a verification code, but no twoFactor callback was given"))?;
    let params = serde_wasm_bindgen::to_value(&params).map_err(|e| report!("{e}"))?;
    let mut answer = callback.call1(&JsValue::NULL, &params).map_err(|e| report!("twoFactor: {e:?}"))?;
    if answer.has_type::<js_sys::Promise>() {
        answer = JsFuture::from(js_sys::Promise::from(answer))
            .await
            .map_err(|e| report!("twoFactor: {e:?}"))?;
    }
    if answer.is_null() || answer.is_undefined() {
        return Ok(TwoFactorCallbackResponse::Abort);
    }
    if let Some(code) = answer.as_string().filter(|s| s.chars().all(|c| c.is_ascii_digit())) {
        return Ok(TwoFactorCallbackResponse::SubmitCode(code));
    }
    serde_wasm_bindgen::from_value(answer).map_err(|e| report!("twoFactor returned an unknown answer: {e}"))
}

/// rootcause prints a tree of contexts with source locations; the page wants the
/// innermost cause (Apple's message) with the outermost context for orientation.
fn report_error(report: Report) -> JsError {
    let text = format!("{report}");
    let causes: Vec<&str> = text
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix('●'))
        .map(|line| line.trim().trim_matches('"'))
        .collect();
    match causes.as_slice() {
        [] => JsError::new(text.trim()),
        [only] => JsError::new(only),
        [first, .., last] => JsError::new(&format!("{first}: {last}")),
    }
}

fn option(options: &JsValue, name: &str) -> Option<JsValue> {
    js_sys::Reflect::get(options, &name.into()).ok().filter(|v| !v.is_undefined() && !v.is_null())
}

type FerrySideloader = Sideloader<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>;

/// A signed-in Apple ID with its developer session.
#[wasm_bindgen]
pub struct AppleAccount {
    email: String,
    name: String,
    sideloader: FerrySideloader,
}

#[wasm_bindgen]
impl AppleAccount {
    /// Signs in. `options`: `anisetteUrl` (a SideStore v3 anisette server),
    /// `twoFactor(params)` (returns the code, `"SendToDevices"` for a new code or null
    /// to abort; codes go to trusted devices, not SMS), `revokeCertificates` (when the account
    /// already has the maximum number of development certificates, revoke the
    /// oldest instead of failing), `clientInfo`/`userAgent` (the device Apple is told
    /// about; defaults to the anisette server's). The anisette identity in
    /// localStorage is registered with Apple under one client info: clear
    /// `isideload:anisette_state` when changing it.
    pub async fn login(email: String, password: String, options: JsValue) -> Result<AppleAccount, JsError> {
        let _ = isideload::init();
        let anisette_url = option(&options, "anisetteUrl")
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| DEFAULT_ANISETTE.into());
        let two_factor = option(&options, "twoFactor").map(js_sys::Function::from);
        TWO_FACTOR.with(|c| *c.borrow_mut() = two_factor);
        let revoke = option(&options, "revokeCertificates").is_some_and(|v| v.is_truthy());

        // The device Apple is told about; the anisette server's own when not given.
        let client_info = option(&options, "clientInfo").and_then(|v| v.as_string()).map(|client_info| AnisetteClientInfo {
            client_info,
            user_agent: option(&options, "userAgent").and_then(|v| v.as_string()).unwrap_or_default(),
        });
        let anisette = RemoteV3AnisetteProvider::new(&anisette_url, Box::new(LocalStorage), "0".into())
            .map_err(report_error)?
            .set_client_info_override(client_info);
        let mut account = Account::builder(&email)
            .anisette_provider(anisette)
            .login(&password, ask_two_factor)
            .await
            .map_err(report_error)?;
        TWO_FACTOR.with(|c| *c.borrow_mut() = None);
        let name = account.get_name().map(|(first, last)| format!("{first} {last}")).unwrap_or_default();
        let session = DeveloperSession::from_account(&mut account).await.map_err(report_error)?;

        let sideloader = SideloaderBuilder::<MaxCertsCallbackBox, ExtensionsBehaviorCallbackBox>::new(
            session,
            email.clone(),
        )
        .team_selection(TeamSelection::First)
        .max_certs_behavior(if revoke { MaxCertsBehavior::Revoke } else { MaxCertsBehavior::Error })
        .extensions_behavior(ExtensionsBehaviorSelection::Fixed(ExtensionsBehavior::RegisterAll))
        .storage(Box::new(LocalStorage))
        .machine_name(MACHINE_NAME.into())
        .build();
        Ok(AppleAccount { email, name, sideloader })
    }

    #[wasm_bindgen(getter)]
    pub fn email(&self) -> String {
        self.email.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The developer team apps get signed for: `{ teamId, name }`.
    pub async fn team(&mut self) -> Result<JsValue, JsError> {
        let team = self.sideloader.get_team().await.map_err(report_error)?;
        let out = js_sys::Object::new();
        js_sys::Reflect::set(&out, &"teamId".into(), &team.team_id.into()).ok();
        js_sys::Reflect::set(&out, &"name".into(), &team.name.unwrap_or_default().into()).ok();
        Ok(out.into())
    }

    /// Registers the device, creates the App IDs and profile, signs `ipa` with this
    /// account's development certificate and installs it. `progress(fraction)` is
    /// called while signing. Returns the special app it recognized
    /// ("SideStore", "LiveContainer", ...; their pairing file is embedded) or null.
    pub async fn install(
        &mut self,
        device: &IosDevice,
        ipa: Vec<u8>,
        progress: Option<js_sys::Function>,
    ) -> Result<Option<String>, JsError> {
        use isideload_vfs::fs;
        // isideload reads and writes the bundle through its virtual file system.
        isideload_vfs::set_vfs(Box::new(isideload_vfs::memory::MemoryVfs::new()));
        let path = PathBuf::from("/ferry/app.ipa");
        fs::create_dir_all("/ferry").map_err(js_error)?;
        fs::write(&path, &ipa).map_err(js_error)?;
        drop(ipa);

        let pairing = device.pairing_record()?;
        let provider = device.provider();
        let report = |fraction: f32| {
            if let Some(progress) = &progress {
                let _ = progress.call1(&JsValue::NULL, &JsValue::from_f64(fraction as f64));
            }
            std::future::ready(())
        };
        let special = self
            .sideloader
            .install_app(&provider, path, false, Some(report), pairing.as_deref())
            .await
            .map_err(report_error);
        let _ = fs::remove_dir_all("/ferry");
        Ok(special?.map(|app| format!("{app:?}")))
    }
}
