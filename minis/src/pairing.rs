use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use idevice::remote_pairing::{
    PairableHost, PairableHostInfo, PeerDevice, RpPairingFile, RpPairingSocket,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::bridge::{Advertisement, Bridge, BridgeError, PAIRABLE_HOST_TYPE};
use crate::fail::{chain, Fail, Result};
use crate::keepalive::{self, KeepAlive};
use crate::{tunnel, ui, Args};

const HANDOFF_PORT: u16 = 61207;
const CODE_TRIES: u32 = 3;
const MAX_FILE: usize = 64 * 1024;
const ANSWER_WAIT: Duration = Duration::from_secs(20);

pub const HOST_NAME: &str = "siboot";
const HOST_MODEL: &str = "Mac17,7";

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub device: Duration,
    pub code_request: Duration,
    pub code_entry: Duration,
}

pub const LIMITS: Limits = Limits {
    device: Duration::from_secs(10 * 60),
    code_request: Duration::from_secs(30),
    code_entry: Duration::from_secs(5 * 60),
};

const MAX_ATTEMPTS: u32 = 3;

const RETURN_WAIT: Duration = Duration::from_secs(10 * 60);

pub const UPDATE_OR_ROOTSHELL: &str = "Обновите Open Minis через App Store до версии 1.16 или новее и снова запустите siboot. Или выполните сопряжение через rootshell (App Store): включите Settings › Location Diary Mode › Session Only, затем в локальном терминале выполните по одной команде: `curl -fsSLO https://raw.githubusercontent.com/ripcats/siboot-ru/main/rootshell/bin/siboot-pair.wasm`, `ifconfig en0 > ifconfig.txt`, `./siboot-pair.wasm`. После сопряжения оставьте rootshell запущенным и снова запустите siboot здесь: он запросит код передачи, показанный rootshell.";

pub enum Source {
    Saved,
    Rootshell,
    Minis,
}

pub async fn ensure(args: &Args, path: &Path) -> Result<Source> {
    let mut saved_refused = None;
    if path.exists() {
        ui::activity("проверка файла сопряжения с прошлого запуска");
        match tunnel::verify(args.device, path).await {
            Ok(()) => {
                ui::ok("iPhone принимает файл сопряжения с прошлого запуска");
                return Ok(Source::Saved);
            }
            Err(f) => {
                ui::note("iPhone не принимает сохранённый файл сопряжения; siboot выполнит сопряжение заново.");
                ui::detail(&f.raw);
                saved_refused = Some(f);
            }
        }
    }

    let bridge = Bridge::from_env();
    ui::activity("проверка возможности сопряжения через Open Minis");
    let here = can_pair_here(&bridge).await;

    let offer = SocketAddr::from(([127, 0, 0, 1], HANDOFF_PORT));
    let hint = if here.is_ok() {
        "введите показанный код передачи или отмените, чтобы выполнить сопряжение в Minis"
    } else {
        "введите показанный код передачи"
    };
    let ask = || {
        ui::ask_code(
            "rootshell предлагает файл сопряжения",
            "Код",
            hint,
            "Код передачи из rootshell: ",
        )
    };
    match take(offer, path, ask, ANSWER_WAIT).await? {
        Taken::File => {
            ui::activity("проверка файла на iPhone");
            tunnel::verify(args.device, path).await?;
            ui::ok("iPhone принимает файл сопряжения из rootshell");
            return Ok(Source::Rootshell);
        }
        Taken::Cancelled if here.is_err() => {
            return Err(Fail::by_user("Остановлено на запросе кода передачи."))
        }
        Taken::Cancelled | Taken::NotOffered => {}
    }

    let notify = match here {
        Ok(notify) => notify,
        Err(why) => {
            let raw = match saved_refused {
                Some(f) => format!("сохранённый файл сопряжения отклонён: {}", f.raw),
                None => format!(
                    "на {offer} файл не предлагается, отсутствует {}",
                    path.display()
                ),
            };
            return Err(Fail::new(format!("{why} {UPDATE_OR_ROOTSHELL}"), raw));
        }
    };
    ui::ok("Open Minis может выполнить сопряжение с iPhone");
    pair_here(args, path, &bridge, notify).await?;
    ui::activity("проверка нового сопряжения на iPhone");
    tunnel::verify(args.device, path).await.map_err(|f| {
        Fail::new(
            format!(
                "Сопряжение выполнено, но iPhone не принимает его: {}",
                f.advice
            ),
            f.raw,
        )
    })?;
    ui::ok("iPhone принимает новое сопряжение");
    Ok(Source::Minis)
}

pub async fn can_pair_here(bridge: &Bridge) -> std::result::Result<bool, String> {
    let types = match bridge.bonjour_types().await {
        Ok(types) => types,
        Err(BridgeError::Missing(_)) => {
            return Err("Эта версия Open Minis не может выполнить сопряжение с iPhone; функция появилась в Minis 1.16."
                .to_string())
        }
        Err(e) => return Err(format!("minis-bonjour в Open Minis не ответил ({e}).")),
    };
    if !types.declares(PAIRABLE_HOST_TYPE) {
        return Err("Эта версия Open Minis не может объявить узел сопряжения.".to_string());
    }
    ui::detail(&format!(
        "minis-bonjour может объявлять {PAIRABLE_HOST_TYPE} (типов объявлено: {}, {})",
        types.types.len(),
        if types.restricted {
            "только объявленные"
        } else {
            "разрешены любые типы"
        }
    ));
    let notify = match bridge.notification_status().await {
        Ok(status) => {
            ui::detail(&format!("уведомления Minis: {status}"));
            status != "denied"
        }
        Err(e) => {
            ui::detail(&format!(
                "не удалось прочитать настройки уведомлений Minis: {e}"
            ));
            false
        }
    };
    Ok(notify)
}

async fn pair_here(args: &Args, path: &Path, bridge: &Bridge, notify: bool) -> Result<()> {
    let identity = Identity::load(path, &args.home).await;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| {
            Fail::new(
                "Не удалось открыть порт для ожидания подключения.",
                e.to_string(),
            )
        })?;
    let setup = HostSetup {
        bridge: bridge.clone(),
        advertise: true,
        name: HOST_NAME.to_string(),
        keepalive_dir: Some(args.home.clone()),
        notify,
        limits: LIMITS,
        pin_hook: None,
        player_check: PLAYER_CHECK,
    };
    let paired = run_host(listener, setup, identity, path, &args.home).await?;
    ui::ok(&format!(
        "сопряжение с {} выполнено ({})",
        paired.name, paired.model
    ));
    ui::detail(&format!("RemotePairing UDID: {}", paired.udid));
    Ok(())
}

pub struct Identity {
    pub file: RpPairingFile,
    pub alt_irk: Option<[u8; 16]>,
}

impl Identity {
    pub async fn load(path: &Path, home: &Path) -> Self {
        let file = match RpPairingFile::read_from_file(path).await {
            Ok(mut existing) => {
                existing.alt_irk = None;
                ui::detail("повторно используется ключ узла из сохранённого файла сопряжения");
                existing
            }
            Err(_) => {
                let mut file = RpPairingFile::generate(HOST_NAME);
                file.identifier = uuid::Uuid::new_v4().to_string().to_uppercase();
                file
            }
        };
        let alt_irk = std::fs::read_to_string(alt_irk_path(home))
            .ok()
            .and_then(|hex| parse_alt_irk(hex.trim()));
        Self { file, alt_irk }
    }
}

fn alt_irk_path(home: &Path) -> PathBuf {
    home.join("host_alt_irk")
}

pub struct HostSetup {
    pub bridge: Bridge,
    pub advertise: bool,
    pub name: String,
    pub keepalive_dir: Option<PathBuf>,
    pub notify: bool,
    pub limits: Limits,
    pub pin_hook: Option<mpsc::UnboundedSender<String>>,
    pub player_check: Duration,
}

const PLAYER_CHECK: Duration = Duration::from_secs(3);

pub struct Paired {
    pub name: String,
    pub model: String,
    pub udid: String,
}

pub async fn run_host(
    listener: TcpListener,
    setup: HostSetup,
    identity: Identity,
    out_path: &Path,
    home: &Path,
) -> Result<Paired> {
    let Identity {
        file: mut pairing_file,
        alt_irk,
    } = identity;
    let port = listener
        .local_addr()
        .map_err(|e| {
            Fail::new(
                "Не удалось открыть порт для ожидания подключения.",
                e.to_string(),
            )
        })?
        .port();

    let mut host_info = PairableHostInfo::generate(setup.name.clone(), HOST_MODEL);
    if let Some(alt_irk) = alt_irk {
        host_info.alt_irk = alt_irk;
    }
    let service_id = pairing_file.identifier.clone();
    let txt = host_info.mdns_txt_records(&service_id);

    let steps = ready_steps(&setup);
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let notes: &[&str] = if setup.keepalive_dir.is_some() {
        &[PLAYER_NOTE]
    } else {
        &[]
    };
    if setup.advertise {
        withdraw_leftovers(&setup.bridge).await;
    }
    let advert = loop {
        ui::status("начните сопряжение, когда будете готовы");
        if !ui::get_ready(READY_TITLE, &steps, notes, "Начать сопряжение") {
            return Err(Fail::by_user("Остановлено до начала сопряжения."));
        }
        if !setup.advertise {
            break None;
        }
        match publish(&setup, &service_id, port, &txt).await {
            Ok(advert) => break Some(advert),
            Err(e) => match fixable(&e) {
                Some(advice) => {
                    ui::warn(advice);
                    ui::detail(&e.to_string());
                }
                None => return Err(advertise_failed(e)),
            },
        }
    };

    if setup.notify && setup.advertise {
        notify(
            &setup.bridge,
            &format!("«Pair with {}» готово", setup.name),
            "Настройки › Конфиденциальность и безопасность › Режим разработчика, внизу страницы.",
        );
    }
    let mut keepalive = match &setup.keepalive_dir {
        Some(dir) => match KeepAlive::start(&setup.bridge, dir, keepalive::PAIRING_SECONDS).await {
            Ok(k) => {
                ui::detail("apple-player воспроизводит тишину, чтобы iOS не останавливала Minis");
                Some(k)
            }
            Err(e) => {
                ui::warn(
                    "Не удалось запустить тишину для фоновой работы Minis. iOS может приостановить приложение до появления пункта Pair with siboot.",
                );
                ui::detail(&e);
                None
            }
        },
        None => None,
    };

    let waiting = format!(
        "ожидание «Pair with {}» в Настройках (до {})",
        setup.name,
        spoken(setup.limits.device)
    );
    ui::status(&waiting);
    let deadline = tokio::time::Instant::now() + setup.limits.device;
    let mut attempt = 0;
    let peer: PeerDevice = loop {
        attempt += 1;
        let accepted = loop {
            tokio::select! {
                accepted = tokio::time::timeout_at(deadline, listener.accept()) => break accepted,
                () = tokio::time::sleep(setup.player_check), if keepalive.is_some() => {
                    if let Some(k) = keepalive.as_mut() {
                        keep_playing(k, &setup).await;
                    }
                }
            }
        };
        let stream = match accepted {
            Ok(Ok((stream, peer))) => {
                ui::status("Настройки подключились: ожидание запроса кода");
                ui::detail(&format!("подключение от {peer}"));
                stream
            }
            Ok(Err(e)) => {
                return Err(Fail::new(
                    "Не удалось принять соединение от iPhone.",
                    e.to_string(),
                ))
            }
            Err(_) => {
                return Err(Fail::new(
                    format!(
                    "За {} никто не нажал Pair with {}. Если пункт не появился в Настройках, включите Wi-Fi, разрешите Open Minis доступ к локальной сети (Настройки › Конфиденциальность и безопасность › Локальная сеть), не закрывайте плеер и снова запустите siboot.",
                        setup.name,
                        spoken(setup.limits.device)
                    ),
                "нет соединения с портом сопряжения",
                ))
            }
        };

        let socket = RpPairingSocket::new_device(stream);
        let mut host = PairableHost::new(socket, host_info.clone());
        let (code_shown_tx, code_shown) = oneshot::channel();
        let (code_seen_tx, mut code_seen) = oneshot::channel::<()>();
        let bridge = setup.bridge.clone();
        let posts = setup.notify;
        let hook = setup.pin_hook.clone();
        let ended = {
            let conversation = host.accept(&mut pairing_file, move |pin: String| async move {
                ui::status("ожидание ввода кода в Настройках");
                ui::show_code(
                    &pin,
                    if posts {
                        "Введите его в Настройках. Код также отправлен в уведомлении."
                    } else {
                        "Введите его в Настройках."
                    },
                );
                if posts {
                    notify(
                        &bridge,
                        "Код сопряжения siboot",
                        &format!("{} — введите его в Настройках", ui::spaced(&pin)),
                    );
                }
                if let Some(hook) = hook {
                    let _ = hook.send(pin);
                }
                let _ = code_seen_tx.send(());
                let _ = code_shown_tx.send(());
            });
            within_limits(conversation, code_shown, setup.limits).await
        };
        ui::code_gone();
        let code_was_seen = code_seen.try_recv().is_ok();
        let (failure, note) = match ended {
            Ok(Ok(peer)) => break peer,
            Err(timeout) if code_was_seen => return Err(timeout),
            Err(quiet) => (quiet, "Соединение прервалось до запроса кода."),
            Ok(Err(e)) if !code_was_seen => {
                (pairing_failed(&e), "Соединение закрылось до запроса кода.")
            }
            Ok(Err(e)) if is_wrong_code(&e) => (
                pairing_failed(&e),
                "Код не совпал. Снова нажмите этот пункт в Настройках, чтобы получить новый.",
            ),
            Ok(Err(e)) => (
                pairing_failed(&e),
                "Сопряжение не завершено. Снова нажмите этот пункт в Настройках.",
            ),
        };
        if attempt >= MAX_ATTEMPTS {
            return Err(failure);
        }
        ui::warn(note);
        ui::detail(&failure.raw);
        ui::status(&waiting);
    };
    drop(listener);

    save(out_path, &pairing_file.to_bytes())?;
    ui::detail(&format!("файл сопряжения записан: {}", out_path.display()));
    if let Err(e) = std::fs::write(alt_irk_path(home), hex::encode(host_info.alt_irk)) {
        ui::detail(&format!("не удалось сохранить altIRK узла: {e}"));
    }

    if let Some(advert) = advert {
        advert.stop().await;
    }
    if let Some(k) = keepalive {
        wait_for_return(&setup).await;
        k.stop().await;
    }

    Ok(Paired {
        name: peer.name,
        model: peer.model,
        udid: peer.remotepairing_udid,
    })
}

const READY_TITLE: &str = "Сопряжение iPhone с siboot";
const PLAYER_NOTE: &str = "После начала поверх Minis откроется плеер с тишиной. Он не даст iOS приостановить Minis в Настройках — не закрывайте его. siboot остановит плеер после возвращения.";

fn ready_steps(setup: &HostSetup) -> Vec<String> {
    vec![
        "Откройте Настройки › Конфиденциальность и безопасность › Режим разработчика и прокрутите вниз.".into(),
        format!(
            "Нажмите Pair with {}. Пункт может появиться через несколько секунд.",
            setup.name
        ),
        if setup.notify {
            "Введите код-пароль iPhone, затем шестизначный код siboot из уведомления."
                .into()
        } else {
            "Введите код-пароль iPhone, затем вернитесь в Minis, чтобы увидеть шестизначный код siboot: уведомления Minis выключены."
                .into()
        },
        "Когда Настройки сообщат о сопряжении, вернитесь в Minis.".into(),
    ]
}

async fn publish(
    setup: &HostSetup,
    service_id: &str,
    port: u16,
    txt: &[(String, String)],
) -> std::result::Result<Advertisement, BridgeError> {
    let advert = setup
        .bridge
        .advertise(PAIRABLE_HOST_TYPE, service_id, port, txt)
        .await?;
    let a = advert.advert().clone();
    if a.name != service_id {
        advert.stop().await;
        return Err(BridgeError::Refused {
            code: "renamed".into(),
            message: format!("asked for {service_id}, got {}", a.name),
        });
    }
    ui::ok(&format!(
        "«Pair with {}» отображается в Настройках",
        setup.name
    ));
    ui::detail(&format!(
        "{PAIRABLE_HOST_TYPE} {} on Minis's port {} → port {port} here",
        a.name,
        a.listener_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "?".into())
    ));
    if let Some(w) = a.warning {
        ui::detail(&format!("minis-bonjour: {w}"));
    }
    Ok(advert)
}

async fn keep_playing(keepalive: &mut KeepAlive, setup: &HostSetup) {
    let in_front = setup
        .bridge
        .bonjour_list()
        .await
        .map(|l| !l.app_in_background)
        .unwrap_or(true);
    if !in_front {
        return;
    }
    match keepalive.revive().await {
        Ok(false) => {}
        Ok(true) => ui::warn(
            "Плеер был закрыт. Он не даёт iOS приостановить Minis в Настройках, поэтому siboot открыл его снова. Не закрывайте его.",
        ),
        Err(e) => ui::detail(&format!("не удалось снова запустить воспроизведение тишины: {e}")),
    }
}

async fn withdraw_leftovers(bridge: &Bridge) {
    let listing = match bridge.bonjour_list().await {
        Ok(listing) => listing,
        Err(e) => {
            ui::detail(&format!("ошибка minis-bonjour list: {e}"));
            return;
        }
    };
    for advert in listing
        .advertisements
        .iter()
        .filter(|a| a.is_pairable_host())
    {
        ui::detail(&format!(
            "снятие объявления узла сопряжения с прошлого запуска ({})",
            advert.name
        ));
        if let Err(e) = bridge.stop_advert(&advert.id).await {
            ui::detail(&format!("не удалось снять объявление {}: {e}", advert.id));
        }
    }
}

fn fixable(e: &BridgeError) -> Option<&'static str> {
    match e.code() {
        Some("authorization_denied") => Some(
            "Open Minis нужен доступ к локальной сети, чтобы показать Pair with siboot. Разрешите его по запросу iOS или включите в Настройки › Конфиденциальность и безопасность › Локальная сеть, затем повторите попытку.",
        ),
        Some("disabled") => Some(
            "В Open Minis выключен поиск устройств в локальной сети. Включите его в Minis › Настройки › Сеть и повторите попытку.",
        ),
        _ => None,
    }
}

fn advertise_failed(e: BridgeError) -> Fail {
    let advice = match e.code() {
        Some("not_declared") => {
            format!("Эта версия Open Minis не может объявить узел сопряжения. {UPDATE_OR_ROOTSHELL}")
        }
        Some("renamed") => "Minis опубликовал Pair with siboot под другим именем, с которым Настройки не выполнят сопряжение. Запустите siboot снова."
            .to_string(),
        _ => "Open Minis не удалось показать Pair with siboot в Настройках. Запустите siboot снова.".to_string(),
    };
    Fail::new(advice, e.to_string())
}

fn notify(bridge: &Bridge, title: &str, body: &str) {
    let bridge = bridge.clone();
    let (title, body) = (title.to_string(), body.to_string());
    tokio::spawn(async move {
        if let Err(e) = bridge.notify(&title, &body).await {
            tracing::warn!("не удалось отправить уведомление: {e}");
        }
    });
}

async fn wait_for_return(setup: &HostSetup) {
    let in_background = |setup: &HostSetup| {
        let bridge = setup.bridge.clone();
        async move {
            bridge
                .bonjour_list()
                .await
                .map(|l| l.app_in_background)
                .unwrap_or(false)
        }
    };
    if !in_background(setup).await {
        return;
    }
    ui::status("сопряжение выполнено: вернитесь в Minis, чтобы продолжить");
    if setup.notify {
        notify(
            &setup.bridge,
            "siboot выполнил сопряжение iPhone",
            "Вернитесь в Minis: siboot продолжит работу там.",
        );
    }
    let deadline = tokio::time::Instant::now() + RETURN_WAIT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if !in_background(setup).await {
            tokio::time::sleep(Duration::from_millis(500)).await;
            return;
        }
    }
}

fn pairing_failed(e: &idevice::IdeviceError) -> Fail {
    let advice = if is_wrong_code(e) {
        "Код не совпал. Запустите siboot снова и точно введите новый код."
    } else {
        "Сопряжение не завершено. Запустите siboot снова и повторно нажмите этот пункт в Настройках."
    };
    Fail::new(advice, chain(e))
}

fn is_wrong_code(e: &idevice::IdeviceError) -> bool {
    chain(e).to_ascii_lowercase().contains("srp")
}

async fn within_limits<T>(
    conversation: impl Future<Output = T>,
    code_shown: oneshot::Receiver<()>,
    limits: Limits,
) -> Result<T> {
    tokio::pin!(conversation);
    tokio::select! {
        biased;
        done = &mut conversation => return Ok(done),
        Ok(()) = code_shown => {}
        () = tokio::time::sleep(limits.code_request) => {
            return Err(Fail::new(
                "iPhone подключился, но не запросил код. Снова нажмите этот пункт в Настройках или запустите siboot повторно.",
                format!("истекло время ожидания запроса кода: {}", spoken(limits.code_request)),
            ));
        }
    }
    tokio::time::timeout(limits.code_entry, conversation)
        .await
        .map_err(|_| {
            Fail::new(
                format!(
                    "Код не введён за {}. Запустите siboot снова, чтобы получить новый.",
                    spoken(limits.code_entry)
                ),
                "истекло время ожидания ввода кода",
            )
        })
}

fn spoken(d: Duration) -> String {
    let secs = d.as_secs();
    let (n, forms) = if secs >= 60 && secs.is_multiple_of(60) {
        (secs / 60, ["минута", "минуты", "минут"])
    } else {
        (secs, ["секунда", "секунды", "секунд"])
    };
    let last_two = n % 100;
    let last = n % 10;
    let form = if (11..=14).contains(&last_two) {
        forms[2]
    } else {
        match last {
            1 => forms[0],
            2..=4 => forms[1],
            _ => forms[2],
        }
    };
    format!("{n} {form}")
}

fn parse_alt_irk(text: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(text).ok()?;
    bytes.try_into().ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Taken {
    File,
    NotOffered,
    Cancelled,
}

enum Reply {
    File(Vec<u8>),
    WrongCode,
    Unexpected(String),
}

async fn exchange(stream: &mut TcpStream, code: &str) -> std::io::Result<Reply> {
    stream
        .write_all(format!("SIBOOT1 {code}\n").as_bytes())
        .await?;
    let mut reader = BufReader::new(stream);
    let mut answer = String::new();
    if reader.read_line(&mut answer).await? == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "rootshell закрыл соединение без ответа",
        ));
    }
    let answer = answer.trim();
    if answer == "NO" {
        return Ok(Reply::WrongCode);
    }
    let Some(len) = answer
        .strip_prefix("OK ")
        .and_then(|n| n.parse::<usize>().ok())
    else {
        return Ok(Reply::Unexpected(answer.to_string()));
    };
    if len == 0 || len > MAX_FILE {
        return Ok(Reply::Unexpected(answer.to_string()));
    }
    let mut file = vec![0u8; len];
    reader.read_exact(&mut file).await?;
    Ok(Reply::File(file))
}

async fn take(
    offer: SocketAddr,
    path: &Path,
    mut ask: impl FnMut() -> Option<String>,
    wait: Duration,
) -> Result<Taken> {
    let connect = || timeout(Duration::from_secs(2), TcpStream::connect(offer));
    let Ok(Ok(mut stream)) = connect().await else {
        ui::detail(&format!("на {offer} файл сопряжения не предлагается"));
        return Ok(Taken::NotOffered);
    };
    ui::ok("rootshell предлагает файл сопряжения");
    for attempt in 1..=CODE_TRIES {
        let Some(typed) = ask() else {
            return Ok(Taken::Cancelled);
        };
        let code: String = typed.chars().filter(char::is_ascii_digit).collect();
        ui::activity("отправка кода в rootshell");
        if attempt > 1 {
            stream = match connect().await {
                Ok(Ok(s)) => s,
                _ => {
                    return Err(Fail::new(
                        "rootshell перестал предлагать файл сопряжения. Снова запустите `./siboot-pair.wasm` в rootshell, затем siboot здесь.",
                        "",
                    ))
                }
            };
        }
        let file = match timeout(wait, exchange(&mut stream, &code)).await {
            Err(_) => {
                return Err(Fail::new(
                    "rootshell не отвечает: возможно, iOS приостановила приложение. Убедитесь, что в настройках Location Diary Mode приложения rootshell выбран Session Only, ненадолго откройте rootshell, вернитесь сюда и снова запустите siboot.",
                    format!("нет ответа от {offer} за {} с", wait.as_secs()),
                ))
            }
            Ok(Err(e)) => {
                return Err(Fail::new(
                    "Передача файла из rootshell прервалась. Запустите siboot снова.",
                    e.to_string(),
                ))
            }
            Ok(Ok(Reply::Unexpected(answer))) => {
                return Err(Fail::new("rootshell отправил данные, не являющиеся файлом сопряжения.", answer))
            }
            Ok(Ok(Reply::WrongCode)) => {
                let left = CODE_TRIES - attempt;
                if left > 0 {
                    ui::warn(&format!(
                        "Это не код из rootshell (осталось попыток: {left}).",
                    ));
                }
                continue;
            }
            Ok(Ok(Reply::File(file))) => file,
        };
        if let Err(e) = RpPairingFile::from_bytes(&file) {
            return Err(Fail::new(
                "rootshell отправил данные, не являющиеся файлом сопряжения.",
                e.to_string(),
            ));
        }
        save(path, &file)?;
        ui::ok(&format!(
            "файл сопряжения получен из rootshell ({} байт)",
            file.len()
        ));
        return Ok(Taken::File);
    }
    Err(Fail::new(
        "Код передачи не совпал. Запустите siboot снова и введите код, показанный rootshell.",
        "",
    ))
}

fn save(path: &Path, file: &[u8]) -> Result<()> {
    let failed = |e: std::io::Error| {
        Fail::new(
            format!("Не удалось сохранить файл сопряжения {}.", path.display()),
            e.to_string(),
        )
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(failed)?;
    }
    std::fs::write(path, file).map_err(failed)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(failed)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_iphone;

    const QUICK: Duration = Duration::from_secs(5);

    const QUICK_LIMITS: Limits = Limits {
        device: Duration::from_secs(20),
        code_request: Duration::from_secs(20),
        code_entry: Duration::from_secs(20),
    };

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siboot-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_setup(
        bridge: Bridge,
        advertise: bool,
        pin_hook: Option<mpsc::UnboundedSender<String>>,
    ) -> HostSetup {
        HostSetup {
            bridge,
            advertise,
            name: "siboot test".into(),
            keepalive_dir: None,
            notify: false,
            limits: QUICK_LIMITS,
            pin_hook,
            player_check: Duration::from_millis(200),
        }
    }

    async fn pair_once(home: &Path, bridge: Option<Bridge>) -> (Paired, fake_iphone::Outcome) {
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel();
        let advertise = bridge.is_some();
        let setup = test_setup(bridge.unwrap_or_default(), advertise, Some(pin_tx));
        let identity = Identity::load(&out, home).await;
        let host = run_host(listener, setup, identity, &out, home);
        let phone = fake_iphone::pair(addr, async move { pin_rx.recv().await.unwrap() });
        let (host, phone) = tokio::join!(host, phone);
        (host.expect("host paired"), phone.expect("phone paired"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_iphone_pairs_and_the_file_holds_the_host_it_saw() {
        let home = temp_home("pair");
        let (paired, phone) = pair_once(&home, None).await;
        assert_eq!(paired.name, fake_iphone::NAME);
        assert_eq!(paired.model, fake_iphone::MODEL);

        let saved = RpPairingFile::read_from_file(home.join("rp_pairing_file.plist"))
            .await
            .unwrap();
        assert_eq!(saved.identifier, phone.host_identifier);
        assert_eq!(saved.public_key_bytes(), phone.host_public_key);
        assert_eq!(saved.alt_irk.as_deref(), Some(&fake_iphone::ALT_IRK[..]));
        assert_eq!(
            std::fs::read_to_string(home.join("host_alt_irk"))
                .unwrap()
                .len(),
            32
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.join("rp_pairing_file.plist"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_again_keeps_the_host_identity() {
        let home = temp_home("again");
        let (_, first) = pair_once(&home, None).await;
        let alt_irk = std::fs::read_to_string(home.join("host_alt_irk")).unwrap();
        let (_, second) = pair_once(&home, None).await;
        assert_eq!(first.host_identifier, second.host_identifier);
        assert_eq!(first.host_public_key, second.host_public_key);
        assert_eq!(
            std::fs::read_to_string(home.join("host_alt_irk")).unwrap(),
            alt_irk
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn through_minis_the_advert_matches_and_is_withdrawn() {
        let home = temp_home("bridge");
        let stub = fake_iphone::minis_stub(&home);
        let (_, phone) = pair_once(&home, Some(Bridge::in_dir(&stub))).await;

        let calls = std::fs::read_to_string(home.join("minis-calls.log")).unwrap();
        let lines: Vec<&str> = calls.lines().collect();
        let advertise = lines
            .iter()
            .find(|l| l.starts_with("minis-bonjour advertise"))
            .expect("advertised");
        let words: Vec<&str> = advertise.split(' ').collect();
        assert_eq!(words[2], PAIRABLE_HOST_TYPE);
        assert_eq!(
            words[3], phone.host_identifier,
            "instance name is the identifier"
        );
        for key in [
            "name",
            "identifier",
            "authTag",
            "model",
            "flags",
            "ver",
            "minVer",
        ] {
            assert!(
                words.iter().any(|w| w.starts_with(&format!("{key}="))),
                "TXT {key} missing: {advertise}"
            );
        }
        assert!(words.contains(&format!("identifier={}", phone.host_identifier).as_str()));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("minis-bonjour stop adv-1")),
            "the advertisement wasn't withdrawn: {calls}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pairing_host_left_from_before_is_withdrawn_first() {
        let home = temp_home("leftover");
        let stub = fake_iphone::minis_stub(&home);
        std::fs::write(home.join("minis-leftover"), b"").unwrap();
        pair_once(&home, Some(Bridge::in_dir(&stub))).await;
        let calls = std::fs::read_to_string(home.join("minis-calls.log")).unwrap();
        let withdrawn = calls
            .find("minis-bonjour stop adv-0")
            .expect("the leftover was withdrawn");
        let advertised = calls.find("minis-bonjour advertise").unwrap();
        assert!(withdrawn < advertised, "{calls}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_network_refusal_asks_again_and_pairs() {
        let home = temp_home("denied");
        let stub = fake_iphone::minis_stub(&home);
        std::fs::write(home.join("minis-deny-once"), b"").unwrap();
        pair_once(&home, Some(Bridge::in_dir(&stub))).await;
        let calls = std::fs::read_to_string(home.join("minis-calls.log")).unwrap();
        assert_eq!(
            calls.matches("minis-bonjour advertise").count(),
            2,
            "{calls}"
        );
        assert!(!home.join("minis-deny-once").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refusal_that_cant_be_fixed_in_settings_ends_the_run() {
        let home = temp_home("undeclared");
        let stub = fake_iphone::minis_stub(&home);
        std::fs::write(home.join("minis-undeclared"), b"").unwrap();
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let setup = test_setup(Bridge::in_dir(&stub), true, None);
        let identity = Identity::load(&out, &home).await;
        let failure = run_host(listener, setup, identity, &out, &home)
            .await
            .err()
            .unwrap();
        assert!(
            failure.advice.contains("не может объявить узел сопряжения"),
            "{}",
            failure.advice
        );
        assert!(failure.advice.contains("1.16"), "{}", failure.advice);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_silence_plays_until_the_user_is_back_in_minis() {
        let home = temp_home("back");
        let stub = fake_iphone::minis_stub(&home);
        let marker = home.join("minis-background");
        std::fs::write(&marker, b"").unwrap();
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel();
        let mut setup = test_setup(Bridge::in_dir(&stub), true, Some(pin_tx));
        setup.keepalive_dir = Some(home.clone());
        setup.notify = true;
        let identity = Identity::load(&out, &home).await;
        let host = run_host(listener, setup, identity, &out, &home);
        let log = home.join("minis-calls.log");
        let phone = async {
            let paired = fake_iphone::pair(addr, async move { pin_rx.recv().await.unwrap() }).await;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let calls = std::fs::read_to_string(&log).unwrap();
            assert!(
                !calls.contains("apple-player stop"),
                "the silence stopped with Minis in the background: {calls}"
            );
            std::fs::remove_file(&marker).unwrap();
            paired
        };
        let (host, phone) = tokio::join!(host, phone);
        host.expect("host paired");
        phone.expect("phone paired");
        let calls = std::fs::read_to_string(&log).unwrap();
        let played = calls.find("apple-player play").expect("the silence played");
        let code = calls
            .find("--title siboot pairing code")
            .expect("the code was posted");
        let asked_back = calls
            .find("--title siboot paired this iPhone")
            .expect("asked back to Minis");
        let stopped = calls
            .find("apple-player stop")
            .expect("the silence was stopped");
        assert!(
            played < code && code < asked_back && asked_back < stopped,
            "{calls}"
        );
        let ready = calls
            .find("--title Pair with siboot test is ready")
            .expect("said it's ready");
        assert!(ready < code, "{calls}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_player_is_opened_again() {
        let home = temp_home("closed");
        let stub = fake_iphone::minis_stub(&home);
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel();
        let mut setup = test_setup(Bridge::in_dir(&stub), true, Some(pin_tx));
        setup.keepalive_dir = Some(home.clone());
        let identity = Identity::load(&out, &home).await;
        let host = run_host(listener, setup, identity, &out, &home);
        let log = home.join("minis-calls.log");
        let plays = |log: &Path| {
            std::fs::read_to_string(log)
                .unwrap_or_default()
                .matches("apple-player play")
                .count()
        };
        let phone = async {
            while plays(&log) < 1 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            std::fs::write(home.join("minis-player-closed"), b"").unwrap();
            for _ in 0..100 {
                if plays(&log) >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            fake_iphone::pair(addr, async move { pin_rx.recv().await.unwrap() }).await
        };
        let (host, phone) = tokio::join!(host, phone);
        host.expect("host paired");
        phone.expect("phone paired");
        assert_eq!(plays(&log), 2, "{}", std::fs::read_to_string(&log).unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_code_gets_another_try() {
        let home = temp_home("wrongcode");
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel();
        let setup = test_setup(Bridge::default(), false, Some(pin_tx));
        let identity = Identity::load(&out, &home).await;
        let host = run_host(listener, setup, identity, &out, &home);
        let phones = async move {
            let first = fake_iphone::pair(addr, async { "000000".to_string() }).await;
            let _ignored = pin_rx.recv().await.unwrap();
            let second = fake_iphone::pair(addr, async move { pin_rx.recv().await.unwrap() }).await;
            (first, second)
        };
        let (host, (first, second)) = tokio::join!(host, phones);
        assert!(first.is_err(), "the first phone typed a wrong code");
        second.expect("the second try paired");
        assert_eq!(
            host.expect("host paired on the second try").name,
            fake_iphone::NAME
        );
        assert!(out.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_wrong_codes_end_the_run_and_say_why() {
        let home = temp_home("wrong3");
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let setup = test_setup(Bridge::default(), false, None);
        let identity = Identity::load(&out, &home).await;
        let host = run_host(listener, setup, identity, &out, &home);
        let phones = async move {
            for _ in 0..MAX_ATTEMPTS {
                let paired = fake_iphone::pair(addr, async { "000000".to_string() }).await;
                assert!(paired.is_err(), "a wrong code paired");
            }
        };
        let (host, ()) = tokio::join!(host, phones);
        let failure = host.err().expect("host should give up");
        assert!(
            failure.advice.contains("Код не совпал"),
            "{}",
            failure.advice
        );
        assert!(!out.exists(), "no pairing file for a failed pairing");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_that_drops_before_the_code_doesnt_end_pairing() {
        let home = temp_home("stray");
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(std::net::TcpStream::connect(addr).unwrap());
        let (pin_tx, mut pin_rx) = mpsc::unbounded_channel();
        let setup = test_setup(Bridge::default(), false, Some(pin_tx));
        let identity = Identity::load(&out, &home).await;
        let host = run_host(listener, setup, identity, &out, &home);
        let phone = fake_iphone::pair(addr, async move { pin_rx.recv().await.unwrap() });
        let (host, phone) = tokio::join!(host, phone);
        assert_eq!(
            host.expect("host paired after the stray connection").name,
            fake_iphone::NAME
        );
        phone.expect("phone paired");
        assert!(out.exists());
    }

    #[tokio::test]
    async fn nobody_connecting_ends_with_advice() {
        let home = temp_home("idle");
        let out = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut setup = test_setup(Bridge::default(), false, None);
        setup.limits.device = Duration::from_millis(200);
        let identity = Identity::load(&out, &home).await;
        let failure = run_host(listener, setup, identity, &out, &home)
            .await
            .err()
            .unwrap();
        assert!(
            failure.advice.contains("никто не нажал"),
            "{}",
            failure.advice
        );
    }

    #[tokio::test]
    async fn minis_without_its_bonjour_bridge_says_to_update() {
        let home = temp_home("old-minis");
        let empty = home.join("no-commands");
        std::fs::create_dir_all(&empty).unwrap();
        let why = can_pair_here(&Bridge::in_dir(&empty)).await.err().unwrap();
        assert!(why.contains("1.16"), "{why}");

        let stub = fake_iphone::minis_stub(&home);
        assert_eq!(can_pair_here(&Bridge::in_dir(&stub)).await, Ok(true));
        std::fs::write(home.join("minis-undeclared"), b"").unwrap();
        let why = can_pair_here(&Bridge::in_dir(&stub)).await.err().unwrap();
        assert!(why.contains("не может объявить узел сопряжения"), "{why}");
    }

    #[test]
    fn words_and_irks() {
        assert_eq!(spoken(LIMITS.device), "10 минут");
        assert_eq!(spoken(LIMITS.code_request), "30 секунд");
        assert_eq!(spoken(Duration::from_secs(60)), "1 минута");
        assert_eq!(
            parse_alt_irk("00112233445566778899aabbccddeeff").unwrap()[15],
            0xff
        );
        assert!(parse_alt_irk("0011").is_none());
        assert!(parse_alt_irk("zz112233445566778899aabbccddeeff").is_none());
    }

    fn a_pairing_file() -> Vec<u8> {
        RpPairingFile::generate("siboot test").to_bytes()
    }

    async fn rootshell_offering(code: &'static str, file: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let mut asked = String::new();
                BufReader::new(&mut conn)
                    .read_line(&mut asked)
                    .await
                    .unwrap();
                if asked.trim() == format!("SIBOOT1 {code}") {
                    conn.write_all(format!("OK {}\n", file.len()).as_bytes())
                        .await
                        .unwrap();
                    conn.write_all(&file).await.unwrap();
                    return;
                }
                conn.write_all(b"NO\n").await.unwrap();
            }
        });
        addr
    }

    fn typing(codes: &'static [&'static str]) -> impl FnMut() -> Option<String> {
        let mut codes = codes.iter();
        move || codes.next().map(|c| c.to_string())
    }

    #[tokio::test]
    async fn the_file_comes_over_for_the_code_and_only_this_user_can_read_it() {
        let home = temp_home("take");
        let path = home.join("rp_pairing_file.plist");
        let file = a_pairing_file();
        let offer = rootshell_offering("481516", file.clone()).await;
        assert_eq!(
            take(offer, &path, typing(&["481 516"]), QUICK)
                .await
                .unwrap(),
            Taken::File
        );
        assert_eq!(std::fs::read(&path).unwrap(), file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn a_mistyped_code_gets_another_try() {
        let home = temp_home("retry");
        let path = home.join("rp_pairing_file.plist");
        let offer = rootshell_offering("481516", a_pairing_file()).await;
        let taken = take(offer, &path, typing(&["481517", "481516"]), QUICK)
            .await
            .unwrap();
        assert_eq!(taken, Taken::File);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn three_wrong_codes_say_so_and_keep_the_old_file() {
        let home = temp_home("wrong");
        let path = home.join("rp_pairing_file.plist");
        std::fs::write(&path, b"the old file").unwrap();
        let offer = rootshell_offering("481516", a_pairing_file()).await;
        let failure = take(offer, &path, typing(&["1", "2", "3"]), QUICK)
            .await
            .err()
            .unwrap();
        assert!(failure.advice.contains("не совпал"), "{}", failure.advice);
        assert_eq!(std::fs::read(&path).unwrap(), b"the old file");
    }

    #[tokio::test]
    async fn something_that_isnt_a_pairing_file_is_refused() {
        let home = temp_home("junk");
        let path = home.join("rp_pairing_file.plist");
        let offer = rootshell_offering("481516", b"not a plist".to_vec()).await;
        let failure = take(offer, &path, typing(&["481516"]), QUICK)
            .await
            .err()
            .unwrap();
        assert!(
            failure.advice.contains("не является файлом сопряжения"),
            "{}",
            failure.advice
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_rootshell_that_never_answers_is_given_up_on() {
        let home = temp_home("silent");
        let path = home.join("rp_pairing_file.plist");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let offer = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let (conn, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(conn);
        });
        let started = std::time::Instant::now();
        let failure = take(
            offer,
            &path,
            typing(&["481516"]),
            Duration::from_millis(300),
        )
        .await
        .err()
        .expect("a silent rootshell should end the wait");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            failure.advice.contains("didn't answer"),
            "{}",
            failure.advice
        );
        assert!(!path.exists());
        held.abort();
    }

    #[tokio::test]
    async fn nothing_offering_means_nothing_taken() {
        let home = temp_home("none");
        let path = home.join("rp_pairing_file.plist");
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap()
        };
        assert_eq!(
            take(closed, &path, typing(&[]), QUICK).await.unwrap(),
            Taken::NotOffered
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn no_code_typed_is_a_cancel_not_a_failure() {
        let home = temp_home("cancel");
        let path = home.join("rp_pairing_file.plist");
        let offer = rootshell_offering("481516", a_pairing_file()).await;
        assert_eq!(
            take(offer, &path, typing(&[]), QUICK).await.unwrap(),
            Taken::Cancelled
        );
        assert!(!path.exists());
    }
}
