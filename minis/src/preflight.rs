use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::time::{Duration, Instant};

use idevice::remote_pairing::{RemotePairingClient, RpPairingSocket};
use idevice::IdeviceError;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::bridge::{Bridge, BridgeError};
use crate::fail::{chain, Fail, Result};
use crate::{ui, Args, Command};

pub const MINIS_APP_STORE: &str = "itms-apps://apps.apple.com/app/id6759188481";

pub const OLD_MINIS: &str = "Эта версия Open Minis старее 1.16. Для сопряжения iPhone нужна версия 1.16 или новее. Обновите Open Minis в App Store, откройте приложение и снова запустите siboot.";

pub struct Checked {
    pub device: SocketAddr,
}

pub async fn run(args: &Args) -> Result<Checked> {
    let in_minis = crate::web::available() || std::env::var_os("SIBOOT_MINIS_BIN").is_some();
    if in_minis {
        ui::activity("проверка Open Minis");
        check_minis(&Bridge::from_env()).await?;
        ui::ok("Open Minis версии 1.16 или новее");
    }
    reach_device(args).await
}

pub async fn reach_device(args: &Args) -> Result<Checked> {
    ui::activity(&format!("подключение к iPhone по адресу {}", args.device));
    let (device, found) = reach(args.device, DYNAMIC_PORTS, &KNOCKS).await?;
    ui::ok(&format!(
        "iPhone отвечает по адресу {device} (RPPairing, версия протокола {})",
        found.wire_version
    ));
    if let Some(name) = found.name {
        ui::detail(&format!("имя устройства: «{name}»"));
    }
    if args.command == Command::Check {
        ui::activity("проверка возможности сопряжения через Open Minis");
        match crate::pairing::can_pair_here(&crate::bridge::Bridge::from_env()).await {
            Ok(true) => {
                ui::ok("Open Minis может выполнить сопряжение и показать код в уведомлении")
            }
            Ok(false) => {
                ui::ok("Open Minis может выполнить сопряжение с iPhone");
                ui::warn(
                    "Уведомления Minis выключены; код сопряжения будет виден только в приложении.",
                );
            }
            Err(why) => ui::warn(&format!(
                "{why} Используйте ранее сохранённый файл сопряжения или rootshell."
            )),
        }
    }
    Ok(Checked { device })
}

pub async fn check_minis(bridge: &Bridge) -> Result<()> {
    match bridge.bonjour_types().await {
        Err(BridgeError::Missing(_)) => {
            ui::link("Обновить Open Minis", MINIS_APP_STORE);
            Err(Fail::known(OLD_MINIS))
        }
        _ => Ok(()),
    }
}

pub async fn check_online(args: &Args) -> Result<()> {
    let apple_root = reqwest::Certificate::from_der(isideload::sideload::cert_identity::APPLE_ROOT)
        .map_err(|e| {
            Fail::new(
                "Не удалось загрузить корневой сертификат Apple.",
                e.to_string(),
            )
        })?;
    let client = reqwest::Client::builder()
        .user_agent(format!("siboot/{}", crate::VERSION))
        .timeout(Duration::from_secs(20))
        .add_root_certificate(apple_root)
        .build()
        .map_err(|e| Fail::new("Не удалось запустить HTTPS-клиент.", e.to_string()))?;

    let downloads = args
        .ipa
        .as_deref()
        .is_none_or(|ipa| ipa.starts_with("http"));
    if downloads {
        let url = args.ipa.as_deref().unwrap_or(crate::sign::RELEASE_IPA);
        let product = if args.ipa.is_some() {
            "указанный IPA"
        } else {
            "SideInstaller"
        };
        ui::activity("проверка доступности GitHub");
        match client.head(url).send().await {
            Ok(r) if r.status().is_success() => ui::ok(&format!("файл {product} доступен")),
            Ok(r) => ui::warn(&format!(
                "Сервер ответил кодом {} при проверке {product}.",
                r.status(),
            )),
            Err(e) => {
                return Err(Fail::new(
                    format!("Не удалось загрузить {product}. Проверьте интернет-соединение."),
                    crate::error_text::error_text(&e),
                ))
            }
        }
    }
    ui::activity("проверка сервера входа Apple");
    match client
        .get("https://gsa.apple.com/grandslam/GsService2/lookup")
        .send()
        .await
    {
        Ok(_) => ui::ok("сервер входа Apple доступен"),
        Err(e) => {
            return Err(Fail::new(
                "Не удалось подключиться к серверу входа Apple (gsa.apple.com). Проверьте интернет-соединение.",
                crate::error_text::error_text(&e),
            ))
        }
    }
    Ok(())
}

#[derive(Debug)]
pub struct Found {
    pub wire_version: i64,
    pub name: Option<String>,
}

impl Found {
    fn from_handshake(reply: &plist::Value) -> Self {
        let wire_version = reply
            .as_dictionary()
            .and_then(|d| d.get("wireProtocolVersion"))
            .and_then(|v| v.as_signed_integer())
            .unwrap_or(0);
        let name = reply
            .as_dictionary()
            .and_then(|d| d.get("peerDeviceInfo"))
            .and_then(|v| v.as_dictionary())
            .and_then(|d| d.get("name"))
            .and_then(|v| v.as_string())
            .map(str::to_string);
        Self { wire_version, name }
    }
}

const DYNAMIC_PORTS: RangeInclusive<u16> = 49152..=65535;
const LOCKDOWN_PORT: u16 = 62078;

async fn reach(
    addr: SocketAddr,
    ports: impl IntoIterator<Item = u16>,
    knocks: &Knocks,
) -> Result<(SocketAddr, Found)> {
    let missed = match probe(addr).await {
        Ok(found) => return Ok((addr, found)),
        Err(Miss::Unreachable(f)) => return Err(f),
        Err(Miss::Elsewhere(f)) => f,
    };
    ui::detail(&format!(
        "по адресу {addr} служба сопряжения не найдена: {}",
        missed.raw
    ));
    ui::status("поиск службы сопряжения iPhone на других портах");
    let ports = others(addr.port(), ports);
    let count = ports.len();
    let started = Instant::now();
    let search = search(addr.ip(), ports, knocks).await;
    ui::progress_end();
    let took = started.elapsed().as_secs_f32();
    match search {
        Search::Found(moved, found, knocked) => {
            ui::detail(&format!(
                "служба найдена на порту {} (проверено портов: {knocked} из {count}, время: {took:.1} с)",
                moved.port()
            ));
            Ok((moved, found))
        }
        Search::NotFound { open } => Err(Fail {
            raw: format!(
                "{}; no other port answered as it ({count} knocked on in {took:.1} s; took the \
                 connection: {})",
                missed.raw,
                listed(&open)
            ),
            ..missed
        }),
        Search::Stopped(f) => Err(f),
    }
}

fn others(tried: u16, ports: impl IntoIterator<Item = u16>) -> Vec<u16> {
    ports
        .into_iter()
        .filter(|&p| p != tried && p != LOCKDOWN_PORT)
        .collect()
}

fn listed(ports: &[u16]) -> String {
    if ports.is_empty() {
        return "нет".into();
    }
    let mut sorted = ports.to_vec();
    sorted.sort_unstable();
    sorted
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

enum Miss {
    Elsewhere(Fail),
    Unreachable(Fail),
}

async fn probe(addr: SocketAddr) -> std::result::Result<Found, Miss> {
    let stream = match timeout(Duration::from_secs(8), TcpStream::connect(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) if e.kind() == ErrorKind::ConnectionRefused => {
            return Err(Miss::Elsewhere(Fail::new(
                format!(
                    "iPhone отклонил соединение по адресу {addr}; на других портах служба сопряжения не найдена. Включите режим разработчика (Настройки › Конфиденциальность и безопасность › Режим разработчика) и проверьте Wi-Fi. Если они включены, перезагрузите iPhone, подключите LocalDevVPN и снова запустите siboot."
                ),
                format!("{e} ({:?})", e.kind()),
            )))
        }
        Ok(Err(e)) => return Err(Miss::Unreachable(unreachable(addr, &e))),
        Err(_) => {
            return Err(Miss::Unreachable(Fail::new(
                format!(
                    "По адресу {addr} нет ответа. Подключите LocalDevVPN (статус Connected) и проверьте Wi-Fi."
                ),
                "истекло время ожидания подключения: 8 с",
            )))
        }
    };
    match timeout(Duration::from_secs(10), handshake(stream)).await {
        Ok(Ok(found)) => Ok(found),
        Ok(Err(e)) => Err(Miss::Elsewhere(Fail::new(
            format!(
                "По адресу {addr} соединение принято, но устройство не ответило как iPhone; служба сопряжения на других портах не найдена. Включите режим разработчика (Настройки › Конфиденциальность и безопасность › Режим разработчика) и проверьте, что включён LocalDevVPN."
            ),
            chain(&e),
        ))),
        Err(_) => Err(Miss::Elsewhere(Fail::new(
            format!(
                "Соединение с iPhone по адресу {addr} установлено, но ответа нет; служба сопряжения на других портах не найдена. Перезапустите LocalDevVPN."
            ),
            "истекло время ожидания рукопожатия: 10 с",
        ))),
    }
}

async fn handshake(stream: TcpStream) -> std::result::Result<Found, IdeviceError> {
    let mut client = RemotePairingClient::new(RpPairingSocket::new(stream), "siboot");
    let reply = client.attempt_pair_verify().await?;
    Ok(Found::from_handshake(&reply))
}

struct Knocks {
    width: usize,
    connect: Duration,
    answer: Duration,
}

const KNOCKS: Knocks = Knocks {
    width: 32,
    connect: Duration::from_secs(2),
    answer: Duration::from_secs(3),
};

const SILENT_IN_A_ROW: usize = 64;
const MOST_OPEN: usize = 48;
const MOST_BUSY: usize = 256;

enum Search {
    Found(SocketAddr, Found, usize),
    NotFound { open: Vec<u16> },
    Stopped(Fail),
}

async fn search(ip: IpAddr, ports: Vec<u16>, knocks: &Knocks) -> Search {
    let total = ports.len().max(1);
    let mut queue = VecDeque::from(ports);
    let mut running = JoinSet::new();
    let mut width = knocks.width.max(1);
    let (mut settled, mut shown) = (0usize, None);
    let mut open = Vec::new();
    let (mut silent, mut busy) = (0usize, 0usize);
    loop {
        while running.len() < width {
            let Some(port) = queue.pop_front() else { break };
            running.spawn(knock(
                SocketAddr::new(ip, port),
                knocks.connect,
                knocks.answer,
            ));
        }
        let Some(joined) = running.join_next().await else {
            return Search::NotFound { open };
        };
        let Ok((port, outcome)) = joined else {
            continue;
        };
        match outcome {
            Knock::Answered(found) => {
                return Search::Found(SocketAddr::new(ip, port), found, settled + 1)
            }
            Knock::Closed => silent = 0,
            Knock::Open => {
                silent = 0;
                open.push(port);
                if open.len() > MOST_OPEN {
                    return Search::Stopped(Fail::new(
                        format!(
                        "Слишком много портов по адресу {ip} принимают соединение. Вероятно, отвечает не этот iPhone, а другой VPN или прокси. Отключите его, подключите LocalDevVPN и снова запустите siboot."
                        ),
                        format!(
                            "Портов приняли соединение: {}; ни один не ответил как RPPairing: {}",
                            open.len(),
                            listed(&open)
                        ),
                    ));
                }
            }
            Knock::Silent => {
                silent += 1;
                if silent >= SILENT_IN_A_ROW {
                    return Search::Stopped(Fail::new(
                        "LocalDevVPN перестал передавать соединения во время поиска службы сопряжения. Подключите LocalDevVPN снова (статус Connected) и повторно запустите siboot.",
                        format!(
                            "Соединения подряд без ответа: {silent}; время ожидания: {} с; последний порт: {port}",
                            knocks.connect.as_secs_f32()
                        ),
                    ));
                }
            }
            Knock::Busy(e) => {
                busy += 1;
                if busy > MOST_BUSY {
                    return Search::Stopped(Fail::new(
                        "Open Minis не удалось открыть нужное число соединений для поиска службы сопряжения. Закройте Minis, снова откройте приложение и запустите siboot.",
                        format!("{e} ({:?}), {busy} times", e.kind()),
                    ));
                }
                width = (width / 2).max(4).min(knocks.width.max(1));
                queue.push_back(port);
                continue;
            }
            Knock::Failed(e) => return Search::Stopped(unreachable(SocketAddr::new(ip, port), &e)),
        }
        settled += 1;
        let percent = (settled * 100 / total) as u64;
        if shown != Some(percent) {
            shown = Some(percent);
            ui::percent("проверено портов", percent);
        }
    }
}

enum Knock {
    Answered(Found),
    Closed,
    Open,
    Silent,
    Busy(std::io::Error),
    Failed(std::io::Error),
}

async fn knock(addr: SocketAddr, connect: Duration, answer: Duration) -> (u16, Knock) {
    let outcome = match timeout(connect, TcpStream::connect(addr)).await {
        Err(_) => Knock::Silent,
        Ok(Err(e)) => match e.kind() {
            ErrorKind::TimedOut => Knock::Silent,
            ErrorKind::PermissionDenied
            | ErrorKind::NetworkUnreachable
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkDown => Knock::Failed(e),
            _ if is_busy(&e) => Knock::Busy(e),
            _ => Knock::Closed,
        },
        Ok(Ok(stream)) => match timeout(answer, handshake(stream)).await {
            Ok(Ok(found)) => Knock::Answered(found),
            _ => Knock::Open,
        },
    };
    (addr.port(), outcome)
}

fn is_busy(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::EADDRNOTAVAIL | libc::EAGAIN)
    )
}

fn unreachable(addr: SocketAddr, e: &std::io::Error) -> Fail {
    let advice = match e.raw_os_error() {
        Some(1) => format!(
            "iOS не разрешает Minis подключиться к {addr}. Подключите LocalDevVPN (статус Connected) и используйте его адрес 10.7.0.1."
        ),
        _ => format!("Не удалось подключиться к {addr}. Подключите LocalDevVPN и проверьте Wi-Fi."),
    };
    Fail::new(advice, format!("{e} ({:?})", e.kind()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_iphone;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn an_open_minis_before_1_16_is_told_to_update() {
        let home = std::env::temp_dir().join(format!("siboot-test-minis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let empty = home.join("no-commands");
        std::fs::create_dir_all(&empty).unwrap();

        let old = check_minis(&Bridge::in_dir(&empty)).await.err().unwrap();
        assert_eq!(old.advice, OLD_MINIS);
        assert!(old.known && !old.by_user && old.raw.is_empty());

        let stub = fake_iphone::minis_stub(&home);
        assert!(check_minis(&Bridge::in_dir(&stub)).await.is_ok());
        std::fs::write(home.join("minis-undeclared"), b"").unwrap();
        assert!(check_minis(&Bridge::in_dir(&stub)).await.is_ok());
    }

    const QUICK: Knocks = Knocks {
        width: 32,
        connect: Duration::from_secs(2),
        answer: Duration::from_millis(300),
    };

    async fn listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    async fn pairing_service() -> u16 {
        let (listener, port) = listener().await;
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut head = [0u8; 11];
                    if s.read_exact(&mut head).await.is_err() || &head[..9] != b"RPPairing" {
                        return;
                    }
                    let mut body = vec![0; u16::from_be_bytes([head[9], head[10]]) as usize];
                    if s.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    let reply = serde_json::json!({
                        "message": {"plain": {"_0": {"response": {"_1": {"handshake": {"_0": {
                            "deviceOptions": {"allowsPairSetup": true},
                            "peerDeviceInfo": {"name": "Test iPhone"},
                            "wireProtocolVersion": 26
                        }}}}}}},
                        "originatedBy": "device",
                        "sequenceNumber": 0
                    })
                    .to_string();
                    let mut frame = b"RPPairing".to_vec();
                    frame.extend_from_slice(&(reply.len() as u16).to_be_bytes());
                    frame.extend_from_slice(reply.as_bytes());
                    let _ = s.write_all(&frame).await;
                    let _ = s.read(&mut [0u8; 64]).await;
                });
            }
        });
        port
    }

    async fn silent_service() -> u16 {
        let (listener, port) = listener().await;
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        port
    }

    async fn other_service() -> u16 {
        let (listener, port) = listener().await;
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        port
    }

    async fn closed_port() -> u16 {
        listener().await.1
    }

    fn local(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test]
    async fn a_refused_port_sends_siboot_looking_for_the_service() {
        let (first, closed) = (closed_port().await, closed_port().await);
        let (silent, other, service) = (
            silent_service().await,
            other_service().await,
            pairing_service().await,
        );
        let mut ports = vec![closed; 200];
        ports.extend([silent, other, closed, service]);

        let (at, found) = reach(local(first), ports, &QUICK).await.unwrap();
        assert_eq!(at, local(service));
        assert_eq!(found.wire_version, 26);
        assert_eq!(found.name.as_deref(), Some("Test iPhone"));
    }

    #[tokio::test]
    async fn so_does_something_else_on_that_port() {
        let (other, service) = (other_service().await, pairing_service().await);
        let (at, _) = reach(local(other), [other, service], &QUICK).await.unwrap();
        assert_eq!(at, local(service));
    }

    #[tokio::test]
    async fn the_service_where_it_should_be_is_taken_without_looking() {
        let service = pairing_service().await;
        let (at, found) = reach(local(service), std::iter::empty(), &QUICK)
            .await
            .unwrap();
        assert_eq!((at, found.wire_version), (local(service), 26));
    }

    #[tokio::test]
    async fn nowhere_is_said_with_what_took_the_connection() {
        let first = closed_port().await;
        let (silent, other, closed) = (
            silent_service().await,
            other_service().await,
            closed_port().await,
        );
        let f = reach(local(first), [closed, other, silent], &QUICK)
            .await
            .unwrap_err();
        assert!(f.advice.starts_with(&format!(
            "iPhone отклонил соединение по адресу 127.0.0.1:{first}, остальные порты недоступны"
        )));
        assert!(f.advice.contains("restart the iPhone"));
        assert!(f.raw.contains("Connection refused"), "{}", f.raw);
        let (low, high) = (other.min(silent), other.max(silent));
        assert!(f.raw.contains("3 knocked on"), "{}", f.raw);
        assert!(
            f.raw
                .ends_with(&format!("took the connection: {low}, {high})")),
            "{}",
            f.raw
        );
        assert!(!f.known && !f.by_user);
    }

    #[tokio::test]
    async fn ports_that_all_take_the_connection_are_not_this_iphones() {
        let (first, other) = (closed_port().await, other_service().await);
        let f = reach(local(first), vec![other; MOST_OPEN + 5], &QUICK)
            .await
            .unwrap_err();
        assert!(f.advice.contains("another VPN or a proxy"), "{}", f.advice);
        assert!(f
            .raw
            .starts_with(&format!("{} ports took the connection", MOST_OPEN + 1)));
    }

    #[test]
    fn lockdownd_and_the_port_tried_are_not_knocked_on_again() {
        let ports = others(49152, DYNAMIC_PORTS);
        assert_eq!(ports.len(), 65535 - 49152 + 1 - 2);
        assert_eq!((ports[0], *ports.last().unwrap()), (49153, 65535));
        assert!(!ports.contains(&LOCKDOWN_PORT) && !ports.contains(&49152));
        assert_eq!(others(50000, DYNAMIC_PORTS)[0], 49152);
        assert_eq!(listed(&[54666, 49999]), "49999, 54666");
        assert_eq!(listed(&[]), "none");
    }
}
