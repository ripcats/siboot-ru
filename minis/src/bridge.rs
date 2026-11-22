use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

pub const PAIRABLE_HOST_TYPE: &str = "_remotepairing-pairable-host._tcp";

const CALL_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Default)]
pub struct Bridge {
    dir: Option<PathBuf>,
}

#[derive(Debug)]
pub enum BridgeError {
    Missing(String),
    Refused { code: String, message: String },
    Unreadable(String),
    Failed(String),
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(tool) => write!(f, "`{tool}` не установлена"),
            Self::Refused { code, message } => write!(f, "{code}: {message}"),
            Self::Unreadable(text) => write!(f, "не удалось прочитать ответ: {text}"),
            Self::Failed(why) => write!(f, "{why}"),
        }
    }
}

impl BridgeError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { code, .. } => Some(code),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Advert {
    pub id: String,
    pub service_type: String,
    pub name: String,
    pub state: String,
    pub listener_port: Option<u16>,
    pub guest_port: Option<u16>,
    pub failed_reason: Option<String>,
    pub warning: Option<String>,
}

impl Advert {
    fn from_value(v: &Value) -> Option<Self> {
        let text = |key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
        let port = |key: &str| {
            v.get(key)
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
        };
        Some(Self {
            id: text("id")?,
            service_type: text("type").unwrap_or_default(),
            name: text("name").unwrap_or_default(),
            state: text("state").unwrap_or_default(),
            listener_port: port("listener_port"),
            guest_port: port("guest_port"),
            failed_reason: text("failed_reason"),
            warning: text("warning"),
        })
    }

    pub fn is_pairable_host(&self) -> bool {
        normalize_type(&self.service_type) == PAIRABLE_HOST_TYPE
    }
}

fn normalize_type(t: &str) -> String {
    t.trim_end_matches('.').to_ascii_lowercase()
}

pub struct Types {
    pub types: Vec<String>,
    pub restricted: bool,
}

impl Types {
    pub fn declares(&self, service_type: &str) -> bool {
        let wanted = normalize_type(service_type);
        self.types.iter().any(|t| normalize_type(t) == wanted)
    }
}

pub struct Listing {
    pub advertisements: Vec<Advert>,
    pub app_in_background: bool,
}

impl Bridge {
    pub fn from_env() -> Self {
        Self {
            dir: std::env::var_os("SIBOOT_MINIS_BIN").map(PathBuf::from),
        }
    }

    #[cfg(test)]
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
        }
    }

    fn program(&self, tool: &str) -> PathBuf {
        match &self.dir {
            Some(dir) => dir.join(tool),
            None => PathBuf::from(tool),
        }
    }

    pub async fn call(&self, tool: &str, args: &[&str]) -> Result<Value, BridgeError> {
        let child = Command::new(self.program(tool))
            .args(args)
            .arg("--compact")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => BridgeError::Missing(tool.to_string()),
                _ => BridgeError::Failed(format!("не удалось запустить {tool}: {e}")),
            })?;
        let out = tokio::time::timeout(CALL_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| {
                BridgeError::Failed(format!(
                    "{tool} didn't answer within {} s",
                    CALL_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|e| BridgeError::Failed(format!("{tool}: {e}")))?;
        parse_envelope(&out.stdout, &out.stderr)
    }

    pub fn call_now(&self, tool: &str, args: &[&str]) {
        let _ = std::process::Command::new(self.program(tool))
            .args(args)
            .arg("--compact")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    pub async fn bonjour_types(&self) -> Result<Types, BridgeError> {
        let data = self.call("minis-bonjour", &["types"]).await?;
        let types = data
            .get("types")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|t| t.as_str().or_else(|| t.get("type").and_then(Value::as_str)))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let restricted = data
            .get("restricted")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        Ok(Types { types, restricted })
    }

    pub async fn bonjour_list(&self) -> Result<Listing, BridgeError> {
        let data = self.call("minis-bonjour", &["list"]).await?;
        let advertisements = data
            .get("advertisements")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Advert::from_value).collect())
            .unwrap_or_default();
        let app_in_background = data
            .get("app_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Listing {
            advertisements,
            app_in_background,
        })
    }

    pub async fn advertise(
        &self,
        service_type: &str,
        name: &str,
        port: u16,
        txt: &[(String, String)],
    ) -> Result<Advertisement, BridgeError> {
        let port = port.to_string();
        let records: Vec<String> = txt.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut args = vec!["advertise", service_type, name, port.as_str()];
        args.extend(records.iter().map(String::as_str));
        let data = self.call("minis-bonjour", &args).await?;
        let advert = Advert::from_value(&data).ok_or_else(|| {
            BridgeError::Unreadable(format!("advertise answered without an id: {data}"))
        })?;
        let guard = Advertisement {
            bridge: self.clone(),
            advert: Some(advert),
        };
        if guard.advert().state == "failed" {
            let why = guard
                .advert()
                .failed_reason
                .clone()
                .unwrap_or_else(|| "no reason given".into());
            guard.stop().await;
            return Err(BridgeError::Refused {
                code: "failed".into(),
                message: why,
            });
        }
        Ok(guard)
    }

    pub async fn stop_advert(&self, id: &str) -> Result<(), BridgeError> {
        self.call("minis-bonjour", &["stop", id]).await.map(|_| ())
    }

    pub async fn notification_status(&self) -> Result<String, BridgeError> {
        let data = self.call("apple-notification", &["settings"]).await?;
        Ok(data
            .get("authorization_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    pub async fn notify(&self, title: &str, body: &str) -> Result<(), BridgeError> {
        self.call(
            "apple-notification",
            &["schedule", "--title", title, "--body", body, "--after", "1"],
        )
        .await
        .map(|_| ())
    }

    pub async fn play(&self, file: &Path) -> Result<String, BridgeError> {
        let path = file.to_string_lossy();
        let data = self.call("apple-player", &["play", path.as_ref()]).await?;
        data.get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                BridgeError::Unreadable(format!("play answered without a session_id: {data}"))
            })
    }

    pub async fn stop_player(&self, session: &str) -> Result<(), BridgeError> {
        self.call("apple-player", &["stop", session])
            .await
            .map(|_| ())
    }

    pub async fn player_status(&self, session: &str) -> Result<String, BridgeError> {
        let data = self.call("apple-player", &["status", session]).await?;
        Ok(data
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    pub async fn resume_player(&self, session: &str) -> Result<(), BridgeError> {
        self.call("apple-player", &["resume", session])
            .await
            .map(|_| ())
    }
}

pub struct Advertisement {
    bridge: Bridge,
    advert: Option<Advert>,
}

impl Advertisement {
    pub fn advert(&self) -> &Advert {
        self.advert.as_ref().expect("advert is only taken by stop")
    }

    pub async fn stop(mut self) {
        if let Some(advert) = self.advert.take() {
            if let Err(e) = self.bridge.stop_advert(&advert.id).await {
                tracing::warn!("не удалось снять объявление {}: {e}", advert.id);
            }
        }
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if let Some(advert) = self.advert.take() {
            self.bridge.call_now("minis-bonjour", &["stop", &advert.id]);
        }
    }
}

pub fn parse_envelope(stdout: &[u8], stderr: &[u8]) -> Result<Value, BridgeError> {
    let text = String::from_utf8_lossy(stdout);
    let spanning = || {
        let (start, end) = (text.find('{')?, text.rfind('}')?);
        serde_json::from_str::<Value>(text.get(start..=end)?).ok()
    };
    let envelope = text
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(Value::is_object)
        .or_else(|| spanning().filter(Value::is_object))
        .ok_or_else(|| {
            let shown = if text.trim().is_empty() {
                String::from_utf8_lossy(stderr)
            } else {
                text.clone()
            };
            BridgeError::Unreadable(shown.trim().chars().take(300).collect())
        })?;
    match envelope.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(envelope.get("data").cloned().unwrap_or(Value::Null)),
        Some(false) => {
            let error = envelope.get("error");
            let field = |key: &str| {
                error
                    .and_then(|e| e.get(key))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            Err(BridgeError::Refused {
                code: field("code"),
                message: field("message"),
            })
        }
        None => Err(BridgeError::Unreadable(envelope.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_comes_out_of_a_good_envelope() {
        let out = br#"{"ok":true,"tool":"minis-bonjour","action":"types","timestamp":"t","data":{"types":["_remotepairing-pairable-host._tcp","_http._tcp"],"restricted":true}}"#;
        let data = parse_envelope(out, b"").unwrap();
        assert_eq!(data["types"][0], "_remotepairing-pairable-host._tcp");
    }

    #[test]
    fn a_refusal_keeps_its_code() {
        let out = br#"{"ok":false,"tool":"minis-bonjour","action":"advertise","error":{"code":"not_declared","message":"add it to NSBonjourServices"}}"#;
        match parse_envelope(out, b"") {
            Err(BridgeError::Refused { code, message }) => {
                assert_eq!(code, "not_declared");
                assert!(message.contains("NSBonjourServices"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn pretty_printed_and_noisy_output_still_parses() {
        let out = b"warming up\n{\n  \"ok\": true,\n  \"data\": {\"count\": 0}\n}\n";
        assert_eq!(parse_envelope(out, b"").unwrap()["count"], 0);
        assert!(
            matches!(parse_envelope(b"", b"usage: ..."), Err(BridgeError::Unreadable(t)) if t.contains("usage"))
        );
    }

    #[test]
    fn adverts_read_ios_spellings() {
        let v: Value = serde_json::from_str(
            r#"{"id":"adv-1","kind":"advertise","type":"_remotepairing-pairable-host._tcp.","name":"X (2)","requested_name":"X","guest_port":8080,"listener_port":52311,"state":"ready"}"#,
        )
        .unwrap();
        let advert = Advert::from_value(&v).unwrap();
        assert!(advert.is_pairable_host());
        assert_eq!(
            (advert.listener_port, advert.guest_port),
            (Some(52311), Some(8080))
        );
        assert_eq!(advert.name, "X (2)");
        let types = Types {
            types: vec!["_REMOTEPAIRING-pairable-host._tcp.".into()],
            restricted: true,
        };
        assert!(types.declares(PAIRABLE_HOST_TYPE));
    }
}
