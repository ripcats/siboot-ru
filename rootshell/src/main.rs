mod mdns;
mod rs;
mod ui;

#[cfg(test)]
mod fake_iphone;

use std::cell::Cell;
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::time::Duration;

use idevice::remote_pairing::{
    PairableHost, PairableHostInfo, PeerDevice, RpPairingFile, RpPairingSocket,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_MODEL: &str = "Mac17,7";
const MAX_ATTEMPTS: u32 = 3;
const HOST_NAME: &str = "siboot";
const PAIRING_FILE: &str = "rp_pairing_file.plist";
const ALT_IRK_FILE: &str = "host_alt_irk";
const IFCONFIG_FILE: &str = "ifconfig.txt";
const INSTALL: &str =
    "wget -qO- https://raw.githubusercontent.com/ripcats/siboot-ru/main/minis/install.sh | sh";

struct Args {
    ip: [u8; 4],
    port: Option<u16>,
    service: String,
    plain: bool,
}

fn usage() {
    println!(
        "siboot-pair {VERSION}
Сопрягает этот iPhone с самим собой через rootshell.

ИСПОЛЬЗОВАНИЕ
  ifconfig en0 > ifconfig.txt
  ./siboot-pair.wasm

  Выполняйте по одной строке. Программа
  читает адрес Wi-Fi iPhone из ifconfig.txt
  (WASM-программа не может получить его сама).
  Перед запуском включите rootshell: Settings ›
  Location Diary Mode › Session Only. Это не даст
  rootshell остановиться, пока вы в Настройках.
  После сопряжения файл передаётся siboot
  в Open Minis.

ПАРАМЕТРЫ
  --port N
      Порт для ожидания подключения (по умолчанию случайный).
  --plain
      Обычный текст без строки состояния и крупных цифр.
  -h, --help
      Показать эту справку."
    );
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        ip: [0; 4],
        port: None,
        service: mdns::PAIRABLE_HOST_TYPE.into(),
        plain: false,
    };
    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        let mut value = |flag: &str| {
            argv.next()
                .ok_or(format!("для {flag} нужно указать значение"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "-V" | "--version" => {
                println!("siboot-pair {VERSION}");
                std::process::exit(0);
            }
            "--plain" => args.plain = true,
            "--port" => {
                args.port = Some(
                    value("--port")?
                        .parse()
                        .map_err(|_| "для --port нужно указать число".to_string())?,
                )
            }
            "--service" => args.service = value("--service")?,
            other => return Err(format!("неизвестный аргумент `{other}`")),
        }
    }
    let saved = std::fs::read_to_string(IFCONFIG_FILE).map_err(|_| {
        format!("Файл {IFCONFIG_FILE} не найден. Сначала выполните `ifconfig en0 > {IFCONFIG_FILE}`, затем `./siboot-pair.wasm`.")
    })?;
    args.ip = wifi_address(&saved).ok_or(format!(
        "В {IFCONFIG_FILE} нет IPv4-адреса. Включите Wi-Fi, повторите `ifconfig en0 > {IFCONFIG_FILE}`, затем запустите `./siboot-pair.wasm`."
    ))?;
    Ok(Some(args))
}

fn wifi_address(text: &str) -> Option<[u8; 4]> {
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.next() {
        if word != "inet" {
            continue;
        }
        if let Some(ip) = words
            .peek()
            .and_then(|w| w.trim_start_matches("addr:").parse::<Ipv4Addr>().ok())
            .filter(|ip| !ip.is_loopback() && !ip.is_unspecified() && !ip.is_broadcast())
        {
            return Some(ip.octets());
        }
    }
    None
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            usage();
            return ExitCode::SUCCESS;
        }
        Err(problem) => {
            eprintln!("siboot-pair: {problem}");
            eprintln!("Справка: `./siboot-pair.wasm --help`");
            return ExitCode::from(2);
        }
    };
    ui::init(args.plain);
    let outcome = run(&args);
    let code = match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(problem) => {
            ui::stopped(&problem);
            offer_log();
            ExitCode::FAILURE
        }
    };
    ui::end();
    code
}

const LOG_FILE: &str = "siboot-pair.log";

fn offer_log() {
    if cfg!(test) {
        return;
    }
    let log = ui::recorded().join("\n") + "\n";
    let saved = std::fs::write(LOG_FILE, &log).is_ok();
    let place = format!("Files › rootshell › {LOG_FILE}");
    ui::blank();
    let answer = ui::ask("Скопировать журнал в буфер обмена?", "д/н");
    match answer.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("" | "y" | "yes" | "д" | "да") => {
            ui::copy(&log);
            let lines = log.lines().count();
            let also = if saved {
                format!(" Если вставить не получится, журнал находится здесь: {place}.")
            } else {
                String::new()
            };
            ui::say(
                ui::Mark::Good,
                &format!("Журнал скопирован ({lines} строк).{also}"),
            );
        }
        _ if saved => ui::dim(&format!("Журнал сохранён: {place}.")),
        _ => {}
    }
}

fn run(args: &Args) -> Result<(), String> {
    ui::header(VERSION);
    ui::blank();
    ui::say(
        ui::Mark::Warn,
        "Не выключайте Location Diary (rootshell › Settings › Location Diary Mode › Session Only): этот режим не даст rootshell остановиться, пока вы в Настройках.",
    );
    ui::blank();

    let (mut file, alt_irk) = identity();
    let mut info = PairableHostInfo::generate(HOST_NAME, HOST_MODEL);
    if let Some(alt_irk) = alt_irk {
        info.alt_irk = alt_irk;
    }
    let id = file.identifier.clone();
    let txt = info.mdns_txt_records(&id);

    let port = args.port.unwrap_or_else(random_port);
    let listener =
        rs::Socket::listen(port).map_err(|e| format!("не удалось открыть порт {port}: {e}"))?;
    let host = format!(
        "siboot-{}.local",
        id.split('-').next().unwrap_or("host").to_ascii_lowercase()
    );
    let ad = mdns::Advert {
        service: &args.service,
        instance: &id,
        host: &host,
        ip: args.ip,
        port,
        txt: &txt,
    };

    let announcement = mdns::announcement(&ad, mdns::TTL);
    let mut sent = 0;
    for i in 0..3 {
        match rs::Socket::send_datagram(&announcement, mdns::GROUP, mdns::PORT) {
            Ok(()) => sent += 1,
            Err(e) => ui::say(
                ui::Mark::Warn,
                &format!("Не удалось отправить объявление {}: {e}", i + 1),
            ),
        }
        let dots: String = (0..3).map(|n| if n <= i { '●' } else { '○' }).collect();
        ui::spin(
            &format!("Объявление «Pair with {HOST_NAME}» {dots}"),
            Duration::from_secs(1),
        );
    }
    if sent == 0 {
        return Err("rootshell не удалось отправить объявление Bonjour. Разрешите доступ к локальной сети (Настройки › Конфиденциальность и безопасность › Локальная сеть › rootshell) и запустите программу снова."
            .into());
    }
    let [a, b, c, d] = args.ip;
    ui::settle(
        ui::Mark::Good,
        &format!("Объявление «Pair with {HOST_NAME}» отправлено"),
    );
    ui::say(
        ui::Mark::Info,
        &format!("адрес {a}.{b}.{c}.{d}, порт {port}"),
    );
    ui::blank();
    ui::heading("Теперь на этом iPhone:");
    ui::numbered(1, "Откройте Настройки › Конфиденциальность и безопасность › Режим разработчика и прокрутите вниз.");
    ui::numbered(
        2,
        &format!("Нажмите «Pair with {HOST_NAME}». Пункт может появиться через несколько секунд."),
    );
    ui::numbered(
        3,
        "Введите код-пароль, затем шестизначный код из уведомления.",
    );
    ui::blank();
    ui::live("Ожидание Настроек…");

    let peer = pair(&listener, &info, &mut file, show_code)?;
    drop(listener);

    std::fs::write(PAIRING_FILE, file.to_bytes())
        .map_err(|e| format!("сопряжение выполнено, но не удалось записать {PAIRING_FILE}: {e}"))?;
    let _ = std::fs::write(ALT_IRK_FILE, hex::encode(info.alt_irk));
    let _ = rs::Socket::send_datagram(&mdns::announcement(&ad, 0), mdns::GROUP, mdns::PORT);

    ui::settle(
        ui::Mark::Good,
        &format!("Сопряжение с {} выполнено ({})", peer.name, peer.model),
    );
    ui::say(
        ui::Mark::Good,
        &format!("Файл сопряжения сохранён: Files › rootshell › {PAIRING_FILE}"),
    );
    hand_off(&file.to_bytes())
}

const HANDOFF_PORT: u16 = 61207;
const HANDOFF_TRIES: u32 = 5;

fn hand_off(pairing: &[u8]) -> Result<(), String> {
    let listener = rs::Socket::listen(HANDOFF_PORT).map_err(|e| {
        format!(
            "Сопряжение выполнено, но не удалось передать файл в Minis ({e}). Закройте другие вкладки rootshell с siboot-pair и запустите `./siboot-pair.wasm` снова."
        )
    })?;
    let code = transfer_code();
    ui::blank();
    ui::heading("Затем в Open Minis (App Store):");
    ui::text("В терминале (Minis › Settings › Terminal) запустите установку SideInstaller:");
    ui::blank();
    ui::command(INSTALL);
    ui::blank();
    ui::heading("Когда siboot запросит код, введите этот:");
    ui::big(&code);
    ui::notify(
        "Код передачи siboot",
        &format!(
            "{} — введите его в Minis по запросу siboot",
            ui::spaced(&code)
        ),
    );
    ui::blank();
    ui::dim(
        "siboot запросит код, только если у него ещё нет подходящего файла сопряжения. Если он продолжит без запроса, закройте эту вкладку после завершения Minis. Ctrl-C отменяет передачу.",
    );
    ui::blank();
    ui::live("Передача файла в siboot…");
    if offer(&listener, &code, pairing) {
        ui::settle(
            ui::Mark::Good,
            "siboot получил файл сопряжения. Вкладку rootshell можно закрыть.",
        );
        Ok(())
    } else {
        Err(
            "Передача завершилась. Запустите `./siboot-pair.wasm` снова, чтобы получить новый код."
                .into(),
        )
    }
}

fn offer(listener: &rs::Socket, code: &str, pairing: &[u8]) -> bool {
    let mut wrong = 0;
    while wrong < HANDOFF_TRIES {
        let Ok(conn) = listener.accept() else {
            return false;
        };
        let asked = conn.read_line(64).unwrap_or_default();
        let given: String = asked
            .trim_start_matches("SIBOOT1")
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        if asked.starts_with("SIBOOT1") && given == code {
            let sent = conn
                .write_all(format!("OK {}\n", pairing.len()).as_bytes())
                .and_then(|()| conn.write_all(pairing));
            match sent {
                Ok(()) => return true,
                Err(e) => {
                    ui::say(
                        ui::Mark::Warn,
                        &format!(
                            "Передача файла прервалась ({e}). Ожидание нового запроса от siboot."
                        ),
                    );
                    ui::live("Передача файла в siboot…");
                }
            }
        } else {
            wrong += 1;
            let _ = conn.write_all(b"NO\n");
            let left = HANDOFF_TRIES - wrong;
            if left > 0 {
                ui::say(
                    ui::Mark::Warn,
                    &format!("Неверный код передачи ({wrong} из {HANDOFF_TRIES})."),
                );
                ui::live(&format!("Передача файла: осталось попыток — {left}"));
            }
        }
    }
    false
}

fn transfer_code() -> String {
    let b = uuid::Uuid::new_v4();
    let n = u32::from_be_bytes(b.as_bytes()[..4].try_into().unwrap());
    format!("{}", 100_000 + n % 900_000)
}

fn pair(
    listener: &rs::Socket,
    info: &PairableHostInfo,
    file: &mut RpPairingFile,
    show: impl Fn(&str),
) -> Result<PeerDevice, String> {
    for attempt in 1..=MAX_ATTEMPTS {
        let stream = listener
            .accept()
            .map_err(|e| format!("не удалось принять соединение: {e}"))?;
        ui::live("Настройки подключились, ожидание…");
        let mut host = PairableHost::new(RpPairingSocket::new_device(stream), info.clone());
        let code_shown = Cell::new(false);
        let outcome = futures::executor::block_on(host.accept(file, |pin: String| {
            code_shown.set(true);
            show(&pin);
            async {}
        }));
        let error = match outcome {
            Ok(peer) => return Ok(peer),
            Err(e) => e.to_string(),
        };
        let wrong_code = error.to_ascii_lowercase().contains("srp");
        if attempt == MAX_ATTEMPTS {
            return Err(if wrong_code {
                "Код не совпал. Запустите программу снова и точно введите новый код.".into()
            } else {
                format!("сопряжение не завершено ({error}). Запустите программу снова.")
            });
        }
        ui::settle(
            ui::Mark::Warn,
            match (code_shown.get(), wrong_code) {
                (false, _) => "Соединение закрылось до запроса кода.",
                (true, true) => {
                    "Код не совпал. Снова нажмите этот пункт в Настройках, чтобы получить новый."
                }
                (true, false) => "Сопряжение не завершено. Снова нажмите этот пункт в Настройках.",
            },
        );
        ui::live("Ожидание Настроек…");
    }
    unreachable!("the loop returns on its last attempt")
}

fn show_code(pin: &str) {
    ui::settle(ui::Mark::Good, "Настройки подключились");
    ui::blank();
    ui::heading("Введите этот код в Настройках:");
    ui::big(pin);
    ui::dim("Код также отправлен в уведомлении.");
    ui::blank();
    ui::notify(
        "Код сопряжения siboot",
        &format!("{} — введите его в Настройках", ui::spaced(pin)),
    );
    ui::live("Ожидание проверки кода Настройками…");
}

fn identity() -> (RpPairingFile, Option<[u8; 16]>) {
    let file = match std::fs::read(PAIRING_FILE)
        .ok()
        .and_then(|b| RpPairingFile::from_bytes(&b).ok())
    {
        Some(mut existing) => {
            existing.alt_irk = None;
            existing
        }
        None => fresh_identity(),
    };
    let alt_irk = std::fs::read_to_string(ALT_IRK_FILE)
        .ok()
        .and_then(|h| hex::decode(h.trim()).ok())
        .and_then(|b| b.try_into().ok());
    (file, alt_irk)
}

fn fresh_identity() -> RpPairingFile {
    let mut fresh = RpPairingFile::generate(HOST_NAME);
    fresh.identifier = uuid::Uuid::new_v4().to_string().to_uppercase();
    fresh
}

fn random_port() -> u16 {
    let b = uuid::Uuid::new_v4();
    let n = u16::from_be_bytes([b.as_bytes()[0], b.as_bytes()[1]]);
    50000 + n % 10000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wifi_address_comes_out_of_ifconfig() {
        let ifconfig = "en0: flags=8863<UP,BROADCAST,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
             \tinet6 fe80::1c2b:3a4d:5e6f:7081%en0 prefixlen 64 secured scopeid 0x9\n\
             \tinet 192.168.178.42 netmask 0xffffff00 broadcast 192.168.178.255\n";
        assert_eq!(wifi_address(ifconfig), Some([192, 168, 178, 42]));
        assert_eq!(wifi_address("lo0: inet 127.0.0.1 netmask 0xff000000"), None);
        assert_eq!(
            wifi_address("  10.0.0.7\n"),
            None,
            "only ifconfig output, not a bare address"
        );
        assert_eq!(
            wifi_address("en0: flags=8822<BROADCAST,SMART,SIMPLEX,MULTICAST> mtu 1500"),
            None
        );
    }

    #[test]
    fn transfer_codes_have_six_digits() {
        for _ in 0..100 {
            let code = transfer_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()) && !code.starts_with('0'));
        }
    }

    fn host() -> (
        u16,
        std::sync::mpsc::Receiver<String>,
        std::thread::JoinHandle<(Result<PeerDevice, String>, RpPairingFile)>,
    ) {
        let listener = rs::Socket::listen(0).unwrap();
        let port = listener.local_port();
        let (codes, shown) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let info = PairableHostInfo::generate(HOST_NAME, HOST_MODEL);
            let mut file = fresh_identity();
            let paired = pair(&listener, &info, &mut file, |pin| {
                codes.send(pin.to_string()).unwrap()
            });
            (paired, file)
        });
        (port, shown, thread)
    }

    fn addr(port: u16) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], port))
    }

    async fn next_code(
        shown: std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<String>>>,
    ) -> String {
        tokio::task::spawn_blocking(move || shown.lock().unwrap().recv().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_device_pairs_and_the_file_holds_the_host_it_saw() {
        let (port, shown, host) = host();
        let shown = std::sync::Arc::new(std::sync::Mutex::new(shown));
        let phone = fake_iphone::pair(addr(port), next_code(shown))
            .await
            .expect("phone paired");
        let (paired, file) = host.join().unwrap();
        let peer = paired.expect("host paired");
        assert_eq!(peer.name, fake_iphone::NAME);
        assert_eq!(peer.model, fake_iphone::MODEL);
        assert_eq!(file.identifier, phone.host_identifier);
        assert_eq!(file.public_key_bytes(), phone.host_public_key);
        assert_eq!(file.alt_irk.as_deref(), Some(&fake_iphone::ALT_IRK[..]));
        let reread = RpPairingFile::from_bytes(&file.to_bytes()).unwrap();
        assert_eq!(reread.identifier, file.identifier);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_code_gets_another_try() {
        let (port, shown, host) = host();
        let shown = std::sync::Arc::new(std::sync::Mutex::new(shown));
        let first = fake_iphone::pair(addr(port), async { "000000".to_string() }).await;
        assert!(first.is_err(), "a wrong code paired");
        let _typo = next_code(shown.clone()).await;
        fake_iphone::pair(addr(port), next_code(shown))
            .await
            .expect("the second try paired");
        let (paired, _) = host.join().unwrap();
        assert_eq!(
            paired.expect("host paired on the second try").name,
            fake_iphone::NAME
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn three_wrong_codes_end_the_run_and_say_why() {
        let (port, _shown, host) = host();
        for _ in 0..MAX_ATTEMPTS {
            let paired = fake_iphone::pair(addr(port), async { "000000".to_string() }).await;
            assert!(paired.is_err(), "a wrong code paired");
        }
        let (paired, _) = host.join().unwrap();
        let problem = paired.err().expect("the host should give up");
        assert!(problem.contains("Код не совпал"), "{problem}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connection_that_drops_before_the_code_doesnt_end_pairing() {
        let (port, shown, host) = host();
        drop(std::net::TcpStream::connect(addr(port)).unwrap());
        let shown = std::sync::Arc::new(std::sync::Mutex::new(shown));
        fake_iphone::pair(addr(port), next_code(shown))
            .await
            .expect("phone paired");
        let (paired, _) = host.join().unwrap();
        assert_eq!(
            paired.expect("host paired after the stray connection").name,
            fake_iphone::NAME
        );
    }

    fn ask_for_file(port: u16, code: &str) -> (String, Vec<u8>) {
        use std::io::{BufRead, Read, Write};
        let mut conn = std::net::TcpStream::connect(addr(port)).unwrap();
        conn.write_all(format!("SIBOOT1 {code}\n").as_bytes())
            .unwrap();
        let mut reader = std::io::BufReader::new(conn);
        let mut answer = String::new();
        reader.read_line(&mut answer).unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        (answer.trim().to_string(), rest)
    }

    #[test]
    fn the_file_goes_to_whoever_has_the_code() {
        let listener = rs::Socket::listen(0).unwrap();
        let port = listener.local_port();
        let offered = std::thread::spawn(move || offer(&listener, "481516", b"pairing file"));
        assert_eq!(ask_for_file(port, "000000").0, "NO");
        let (answer, file) = ask_for_file(port, "481516");
        assert_eq!(answer, "OK 12");
        assert_eq!(file, b"pairing file");
        assert!(
            offered.join().unwrap(),
            "the offer ends once siboot has the file"
        );
    }

    #[test]
    fn five_wrong_codes_end_the_offer() {
        let listener = rs::Socket::listen(0).unwrap();
        let port = listener.local_port();
        let offered = std::thread::spawn(move || offer(&listener, "481516", b"pairing file"));
        for _ in 0..HANDOFF_TRIES {
            assert_eq!(ask_for_file(port, "123456").0, "NO");
        }
        assert!(!offered.join().unwrap());
    }
}
