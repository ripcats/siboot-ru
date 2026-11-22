use std::time::Duration;

use idevice::amfi::AmfiClient;
use idevice::lockdown::LockdownClient;
use idevice::{IdeviceError, RsdService};
use tokio::time::timeout;

use crate::fail::{chain, Fail, Result};
use crate::tunnel::{self, Connection};
use crate::ui;

const AMFI_DOMAIN: &str = "com.apple.security.mac.amfi";
const STATUS_KEY: &str = "DeveloperModeStatus";
const REPLY_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Show,
    TurnOn,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    AlreadyOn,
    Shown,
    TurningOn,
    HungUp,
    ShownForPasscode,
}

pub trait Services {
    async fn lockdown(&mut self) -> std::result::Result<LockdownClient, IdeviceError>;
    async fn amfi(&mut self) -> std::result::Result<AmfiClient, IdeviceError>;
}

impl Services for Connection {
    async fn lockdown(&mut self) -> std::result::Result<LockdownClient, IdeviceError> {
        LockdownClient::connect_rsd(&mut self.adapter, &mut self.handshake).await
    }

    async fn amfi(&mut self) -> std::result::Result<AmfiClient, IdeviceError> {
        AmfiClient::connect_rsd(&mut self.adapter, &mut self.handshake).await
    }
}

pub fn unreachable_note(f: Fail) -> Fail {
    let mut advice = format!(
        "{} siboot меняет режим разработчика через это сопряжение и туннель, поэтому они должны работать.",
        f.advice
    );
    if !f.advice.contains("режим разработчика") {
        advice.push_str(
            " Включите режим разработчика в разделе Настройки › Конфиденциальность и безопасность.",
        );
    }
    Fail { advice, ..f }
}

pub async fn run(
    device: &mut impl Services,
    request: Request,
    confirm: impl FnOnce() -> bool,
) -> Result<Outcome> {
    let on = match read_status(device).await {
        Ok(on) => on,
        Err(f) => {
            ui::warn("Не удалось проверить, включён ли режим разработчика; siboot всё равно отправит запрос.");
            ui::detail(&f.raw);
            None
        }
    };
    match on {
        Some(true) => return Ok(Outcome::AlreadyOn),
        Some(false) => ui::status("Режим разработчика выключен"),
        None => {}
    }

    if request == Request::Show {
        return match send(device, Action::Show).await? {
            Sent::Done => Ok(Outcome::Shown),
            Sent::HungUp(raw) => Err(Fail::new(
                "iPhone перестал отвечать до подтверждения запроса. Проверьте раздел Настройки › Конфиденциальность и безопасность; если переключателя нет, снова запустите siboot developer-mode.",
                raw,
            )),
            Sent::Refused(raw) => Err(Fail::new(
                "iPhone отказался показать режим разработчика в Настройках.",
                raw,
            )),
        };
    }

    ui::warn(
        "Без код-пароля iPhone сразу перезагрузится для включения режима разработчика, а Minis остановится. После разблокировки нажмите «Включить» в уведомлении iOS.",
    );
    ui::note("Если установлен код-пароль, iOS откажет, и siboot вместо этого покажет переключатель в Настройках.");
    if !confirm() {
        return Err(Fail::new(
            "Запрос не отправлен, режим разработчика остался выключен. Команда `siboot developer-mode` без --turn-on только показывает переключатель и не перезагружает iPhone.",
            "",
        ));
    }
    match send(device, Action::TurnOn).await? {
        Sent::Done => Ok(Outcome::TurningOn),
        Sent::HungUp(raw) => {
            ui::detail(&raw);
            Ok(Outcome::HungUp)
        }
        Sent::Refused(raw) if raw.to_lowercase().contains("passcode") => {
            ui::note("iOS позволяет включить режим таким способом только без код-пароля.");
            ui::detail(&raw);
            match send(device, Action::Show).await? {
                Sent::Done => Ok(Outcome::ShownForPasscode),
                Sent::HungUp(raw) | Sent::Refused(raw) => Err(Fail::new(
                    "iOS позволяет включить режим таким способом только без код-пароля и не смогла показать переключатель. Снова запустите siboot developer-mode без --turn-on.",
                    raw,
                )),
            }
        }
        Sent::Refused(raw) => Err(Fail::new(
            "iPhone отказался включить режим разработчика.",
            raw,
        )),
    }
}

async fn read_status(device: &mut impl Services) -> Result<Option<bool>> {
    let mut lockdown = tunnel::step("подключение к lockdownd", 15, device.lockdown()).await?;
    let value = tunnel::step(
        "проверка режима разработчика",
        15,
        lockdown.get_value(Some(STATUS_KEY), Some(AMFI_DOMAIN)),
    )
    .await?;
    Ok(value.as_boolean())
}

#[derive(Clone, Copy)]
enum Action {
    Show,
    TurnOn,
}

enum Sent {
    Done,
    HungUp(String),
    Refused(String),
}

async fn send(device: &mut impl Services, action: Action) -> Result<Sent> {
    let mut amfi = tunnel::step("подключение к AMFI на iPhone", 15, device.amfi()).await?;
    let answered = match action {
        Action::Show => timeout(REPLY_WAIT, amfi.reveal_developer_mode_option_in_ui()).await,
        Action::TurnOn => timeout(REPLY_WAIT, amfi.enable_developer_mode()).await,
    };
    Ok(match answered {
        Err(_) => Sent::HungUp(format!("нет ответа за {} с", REPLY_WAIT.as_secs())),
        Ok(Ok(())) => Sent::Done,
        Ok(Err(IdeviceError::Socket(e))) => Sent::HungUp(e.to_string()),
        Ok(Err(e)) => Sent::Refused(chain(&e)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use idevice::Idevice;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    #[derive(Clone, Copy)]
    enum Answer {
        Success,
        Passcode,
        OtherError,
        HangUp,
    }

    struct FakeIphone {
        status: Option<plist::Value>,
        answers: Vec<Answer>,
        sent: Arc<Mutex<Vec<i64>>>,
        lockdown_asked: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeIphone {
        fn new(status: Option<plist::Value>, answers: &[Answer]) -> Self {
            Self {
                status,
                answers: answers.iter().rev().copied().collect(),
                sent: Arc::default(),
                lockdown_asked: Arc::default(),
            }
        }

        fn sent(&self) -> Vec<i64> {
            self.sent.lock().unwrap().clone()
        }
    }

    async fn read_message(stream: &mut DuplexStream) -> Option<plist::Dictionary> {
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.ok()?;
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        stream.read_exact(&mut body).await.ok()?;
        plist::from_bytes(&body).ok()
    }

    async fn write_message(stream: &mut DuplexStream, message: plist::Dictionary) {
        let mut body = Vec::new();
        plist::Value::Dictionary(message)
            .to_writer_xml(&mut body)
            .unwrap();
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
    }

    fn dict(pairs: &[(&str, plist::Value)]) -> plist::Dictionary {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    impl Services for FakeIphone {
        async fn lockdown(&mut self) -> std::result::Result<LockdownClient, IdeviceError> {
            let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
            let status = self.status.clone();
            let asked = self.lockdown_asked.clone();
            tokio::spawn(async move {
                let Some(request) = read_message(&mut theirs).await else {
                    return;
                };
                let field = |k: &str| {
                    request
                        .get(k)
                        .and_then(|v| v.as_string())
                        .unwrap_or("")
                        .to_string()
                };
                asked.lock().unwrap().push((field("Key"), field("Domain")));
                let reply = match status {
                    Some(value) => dict(&[("Request", "GetValue".into()), ("Value", value)]),
                    None => dict(&[
                        ("Request", "GetValue".into()),
                        ("Error", "MissingValue".into()),
                    ]),
                };
                write_message(&mut theirs, reply).await;
            });
            Ok(LockdownClient::new(Idevice::new(Box::new(ours), "siboot")))
        }

        async fn amfi(&mut self) -> std::result::Result<AmfiClient, IdeviceError> {
            let answer = self
                .answers
                .pop()
                .expect("siboot opened more AMFI connections than expected");
            let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
            let sent = self.sent.clone();
            tokio::spawn(async move {
                let Some(request) = read_message(&mut theirs).await else {
                    return;
                };
                let action = request
                    .get("action")
                    .and_then(|v| v.as_signed_integer())
                    .unwrap_or(-1);
                sent.lock().unwrap().push(action);
                let reply = match answer {
                    Answer::Success => dict(&[("success", true.into())]),
                    Answer::Passcode => dict(&[("Error", "Device has a passcode set".into())]),
                    Answer::OtherError => dict(&[("Error", "Something else".into())]),
                    Answer::HangUp => return,
                };
                write_message(&mut theirs, reply).await;
            });
            Ok(AmfiClient::new(Idevice::new(Box::new(ours), "")))
        }
    }

    fn never_asked() -> bool {
        panic!("asked to confirm a restart that shouldn't be sent")
    }

    #[tokio::test]
    async fn already_on_sends_nothing() {
        for request in [Request::Show, Request::TurnOn] {
            let mut iphone = FakeIphone::new(Some(true.into()), &[]);
            let outcome = run(&mut iphone, request, never_asked).await.unwrap();
            assert_eq!(outcome, Outcome::AlreadyOn);
            assert!(iphone.sent().is_empty());
            assert_eq!(
                iphone.lockdown_asked.lock().unwrap().as_slice(),
                [(STATUS_KEY.to_string(), AMFI_DOMAIN.to_string())]
            );
        }
    }

    #[tokio::test]
    async fn off_is_shown_without_asking() {
        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::Success]);
        let outcome = run(&mut iphone, Request::Show, never_asked).await.unwrap();
        assert_eq!(outcome, Outcome::Shown);
        assert_eq!(iphone.sent(), [0]);
    }

    #[tokio::test]
    async fn an_unreadable_status_still_sends_the_request() {
        let mut iphone = FakeIphone::new(None, &[Answer::Success]);
        assert_eq!(
            run(&mut iphone, Request::Show, never_asked).await.unwrap(),
            Outcome::Shown
        );
        assert_eq!(iphone.sent(), [0]);

        let mut iphone = FakeIphone::new(Some("maybe".into()), &[Answer::Success]);
        assert_eq!(
            run(&mut iphone, Request::Show, never_asked).await.unwrap(),
            Outcome::Shown
        );
        assert_eq!(iphone.sent(), [0]);
    }

    #[tokio::test]
    async fn turning_on_needs_a_yes() {
        let mut iphone = FakeIphone::new(Some(false.into()), &[]);
        let f = run(&mut iphone, Request::TurnOn, || false)
            .await
            .unwrap_err();
        assert!(f.advice.contains("Запрос не отправлен"), "{}", f.advice);
        assert!(iphone.sent().is_empty());

        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::Success]);
        let outcome = run(&mut iphone, Request::TurnOn, || true).await.unwrap();
        assert_eq!(outcome, Outcome::TurningOn);
        assert_eq!(iphone.sent(), [1]);
    }

    #[tokio::test]
    async fn a_passcode_refusal_shows_the_switch_instead() {
        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::Passcode, Answer::Success]);
        let outcome = run(&mut iphone, Request::TurnOn, || true).await.unwrap();
        assert_eq!(outcome, Outcome::ShownForPasscode);
        assert_eq!(iphone.sent(), [1, 0]);
    }

    #[tokio::test]
    async fn a_hang_up_after_turning_on_is_the_restart() {
        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::HangUp]);
        let outcome = run(&mut iphone, Request::TurnOn, || true).await.unwrap();
        assert_eq!(outcome, Outcome::HungUp);
        assert_eq!(iphone.sent(), [1]);
    }

    #[tokio::test]
    async fn other_refusals_stop_with_what_the_iphone_said() {
        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::OtherError]);
        let f = run(&mut iphone, Request::TurnOn, || true)
            .await
            .unwrap_err();
        assert!(
            f.advice.contains("отказался включить режим разработчика"),
            "{}",
            f.advice
        );
        assert!(f.raw.contains("Something else"), "{}", f.raw);
        assert_eq!(iphone.sent(), [1]);

        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::OtherError]);
        let f = run(&mut iphone, Request::Show, never_asked)
            .await
            .unwrap_err();
        assert!(
            f.advice.contains("отказался показать режим разработчика"),
            "{}",
            f.advice
        );

        let mut iphone = FakeIphone::new(Some(false.into()), &[Answer::HangUp]);
        let f = run(&mut iphone, Request::Show, never_asked)
            .await
            .unwrap_err();
        assert!(f.advice.contains("перестал отвечать"), "{}", f.advice);
    }

    #[test]
    fn the_note_keeps_the_original_advice_and_cause() {
        let f = unreachable_note(Fail::new(
            "Не удалось подключиться к 10.7.0.1:49152.",
            "refused",
        ));
        assert!(f
            .advice
            .starts_with("Не удалось подключиться к 10.7.0.1:49152. siboot изменяет"));
        assert!(f.advice.ends_with("включите его там."), "{}", f.advice);
        assert_eq!(f.raw, "refused");

        let f = unreachable_note(Fail::new(
            "Включите режим разработчика (Настройки › …).",
            "",
        ));
        assert_eq!(f.advice.matches("Settings ›").count(), 1, "{}", f.advice);
    }
}
