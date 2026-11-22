mod account;
mod bridge;
mod devmode;
mod fail;
#[cfg(test)]
mod fake_iphone;
mod install;
mod keepalive;
mod pairing;
mod preflight;
mod report;
mod saved;
mod seed;
mod sign;
mod team;
mod tunnel;
mod ui;
mod web;

#[allow(dead_code)]
#[path = "../../SideInstaller/rust-core/src/apple_session.rs"]
mod apple_session;
#[allow(dead_code)]
#[path = "../../SideInstaller/rust-core/src/error_text.rs"]
mod error_text;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bridge::Bridge;
use fail::{Fail, Result};
use keepalive::KeepAlive;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const STAGES: usize = 8;
const DEVMODE_STAGES: usize = 4;
const STEP_NAMES: [&str; STAGES] = [
    "Связь с iPhone",
    "Файл сопряжения",
    "Туннель к iPhone",
    "Интернет",
    "Вход в Apple ID",
    "Подпись SideInstaller",
    "Копирование на iPhone",
    "Настройка SideInstaller",
];
const APP_STEP_NAMES: [&str; 7] = [
    "Связь с iPhone",
    "Файл сопряжения",
    "Туннель к iPhone",
    "Интернет",
    "Вход в Apple ID",
    "Подпись приложения",
    "Установка приложения",
];
const CHECK_NAMES: [&str; 2] = ["Связь с iPhone", "Интернет"];
const DEVMODE_NAME: &str = "Dev Mode";

const HANDOFF_NOTES: [&str; 2] = [
    "Беззвучный плеер не даёт iOS приостановить Minis. Не закрывайте его.",
    "Вернитесь в Minis и подтвердите действие.",
];

const DEFAULT_DEVICE_IP: Ipv4Addr = Ipv4Addr::new(10, 7, 0, 1);
const DEFAULT_RP_PORT: u16 = 49152;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    All,
    Check,
    Connect,
    DeveloperMode,
}

#[derive(Clone)]
pub struct Args {
    pub command: Command,
    pub turn_on: bool,
    pub verbose: bool,
    pub plain: bool,
    pub web: Option<bool>,
    pub device: SocketAddr,
    pub home: PathBuf,
    pub pairing_file: Option<PathBuf>,
    pub apple_id: Option<String>,
    pub ipa: Option<String>,
    pub anisette: Option<String>,
}

impl Args {
    pub fn pairing_path(&self) -> PathBuf {
        self.pairing_file
            .clone()
            .unwrap_or_else(|| self.home.join("rp_pairing_file.plist"))
    }
}

fn default_home() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    home.join(".siboot")
}

enum Parsed {
    Run(Args),
    Help,
    Version,
}

fn parse_args(argv: impl IntoIterator<Item = String>) -> std::result::Result<Parsed, String> {
    let mut args = Args {
        command: Command::All,
        turn_on: false,
        verbose: false,
        plain: false,
        web: None,
        device: SocketAddr::new(IpAddr::V4(DEFAULT_DEVICE_IP), DEFAULT_RP_PORT),
        home: default_home(),
        pairing_file: None,
        apple_id: None,
        ipa: None,
        anisette: None,
    };
    let mut argv = argv.into_iter();
    let mut command_seen = false;
    while let Some(arg) = argv.next() {
        let mut value = |flag: &str| argv.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" | "help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "-v" | "--verbose" => args.verbose = true,
            "--plain" => args.plain = true,
            "--web" => args.web = Some(true),
            "--terminal" => args.web = Some(false),
            "--device-ip" => {
                let ip: IpAddr = value("--device-ip")?
                    .parse()
                    .map_err(|_| "--device-ip needs an IP address".to_string())?;
                args.device.set_ip(ip);
            }
            "--port" => {
                let port = value("--port")?
                    .parse()
                    .map_err(|_| "--port needs a port number".to_string())?;
                args.device.set_port(port);
            }
            "--home" => args.home = PathBuf::from(value("--home")?),
            "--pairing-file" => args.pairing_file = Some(PathBuf::from(value("--pairing-file")?)),
            "--apple-id" => args.apple_id = Some(value("--apple-id")?),
            "--ipa" => args.ipa = Some(value("--ipa")?),
            "--anisette" => args.anisette = Some(value("--anisette")?),
            "--turn-on" => args.turn_on = true,
            "check" | "connect" | "developer-mode" if !command_seen => {
                command_seen = true;
                args.command = match arg.as_str() {
                    "check" => Command::Check,
                    "connect" => Command::Connect,
                    _ => Command::DeveloperMode,
                };
            }
            other => return Err(format!("неизвестный аргумент `{other}`")),
        }
    }
    if args.turn_on && args.command != Command::DeveloperMode {
        return Err("--turn-on goes with `siboot developer-mode`".into());
    }
    Ok(Parsed::Run(args))
}

fn usage() {
    use std::io::IsTerminal;
    let styled = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let head = |text: &str| {
        if styled {
            format!("\x1b[1m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    let flag = |text: &str| {
        if styled {
            format!("\x1b[36m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    };
    println!(
        "\
{name} {VERSION}
Устанавливает SideInstaller на этот iPhone через Open Minis с вашей учётной записью Apple.

{usage}
  siboot [command] [options]

  Без команды выполняются все шаги: проверка LocalDevVPN
  и iPhone, сопряжение с siboot (в первый раз — в Настройках),
  подключение, проверка сети, вход в Apple ID, подпись
  и установка SideInstaller, передача ему файла сопряжения.
  С --ipa PATH|URL устанавливается указанное приложение.

{commands}
  {check}
      Проверить LocalDevVPN, iPhone и доступ в интернет.
  {connect}
      Проверить подключение, выполнить сопряжение
      и открыть туннель.
  {devmode}
      Подключиться и показать переключатель режима
      разработчика в Настройках, если он выключен.
      Если он включён, ничего не отправлять. С --turn-on
      запросить включение: iPhone перезагрузится.
      iOS разрешает это только без код-пароля.

{options}
  {apple_id}
      Учётная запись Apple (иначе SIBOOT_APPLE_ID,
      сохранённые записи или запрос). Пароль берётся
      из SIBOOT_PASSWORD или запрашивается, только
      если он нужен Apple.
  {ipa}
      Установить этот IPA вместо SideInstaller.
  {anisette}
      Использовать только этот сервер anisette.
  {turn_on}
      Для developer-mode: включить режим, а не только показать переключатель.
  {device_ip}
      Адрес iPhone в LocalDevVPN (по умолчанию {ip}).
  {port}
      Порт RPPairing на iPhone (по умолчанию {rp}).
      Если ответа нет, siboot проверит другие порты.
  {pairing_file}
      По умолчанию ~/.siboot/rp_pairing_file.plist.
  {home}
      Каталог данных siboot (по умолчанию ~/.siboot).
  {terminal}
      Отвечать в терминале, а не в браузере Minis.
  {plain}
      Обычный текст вместо интерактивного экрана.
  {web}
      Открыть страницу и вне Minis; siboot выведет её адрес.
  {verbose}
      Показывать подробности ошибок протокола и библиотек.
  {help}
      Показать эту справку.

{needs}
  iOS 27 or later, Developer Mode on, Wi-Fi,
  LocalDevVPN (App Store) connected, and
  Open Minis 1.16 or later: siboot stops in
  an older one.",
        name = head("siboot"),
        usage = head("USAGE"),
        commands = head("COMMANDS"),
        options = head("OPTIONS"),
        needs = head("NEEDS"),
        check = flag("check"),
        connect = flag("connect"),
        devmode = flag("developer-mode"),
        apple_id = flag("--apple-id EMAIL"),
        ipa = flag("--ipa PATH|URL"),
        anisette = flag("--anisette URL"),
        turn_on = flag("--turn-on"),
        device_ip = flag("--device-ip IP"),
        port = flag("--port N"),
        pairing_file = flag("--pairing-file PATH"),
        home = flag("--home DIR"),
        plain = flag("--plain"),
        web = flag("--web"),
        terminal = flag("--terminal"),
        verbose = flag("-v, --verbose"),
        help = flag("-h, --help"),
        ip = DEFAULT_DEVICE_IP,
        rp = DEFAULT_RP_PORT,
    );
}

fn main() -> ExitCode {
    let parsed = match parse_args(std::env::args().skip(1)) {
        Ok(p) => p,
        Err(problem) => {
            eprintln!("siboot: {problem}");
            eprintln!("Справка: `siboot --help`");
            return ExitCode::from(2);
        }
    };
    let args = match parsed {
        Parsed::Help => {
            usage();
            return ExitCode::SUCCESS;
        }
        Parsed::Version => {
            println!("siboot {VERSION}");
            return ExitCode::SUCCESS;
        }
        Parsed::Run(args) => args,
    };

    let page = if args.web.unwrap_or_else(web::available) {
        match web::start() {
            Ok(url) => Some(url),
            Err(e) => {
                eprintln!("siboot: не удалось открыть страницу ({e}); продолжаю в терминале");
                None
            }
        }
    } else {
        None
    };
    ui::init(args.verbose, args.plain || page.is_some());
    init_logging(args.verbose);
    ui::header(
        VERSION,
        if args.command == Command::DeveloperMode {
            "Показывает режим разработчика или включает его."
        } else if args.ipa.is_some() {
            "Устанавливает указанное приложение на этот iPhone."
        } else {
            "Устанавливает SideInstaller на этот iPhone."
        },
    );
    if let Some(url) = &page {
        show_page(url);
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            ui::failure("Не удалось запустить среду выполнения.", &e.to_string());
            ui::close();
            return ExitCode::FAILURE;
        }
    };

    watch_interrupt();
    let outcome = runtime.block_on(pipeline(&args));
    let code = match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(f) => {
            ui::failure(&f.advice, &f.raw);
            report::after_failure(&args, &f);
            ExitCode::FAILURE
        }
    };
    ui::close();
    code
}

fn show_page(url: &str) {
    web::on_stop(|| ui::stop_now("Остановлено со страницы."));
    if web::show(url) {
        println!("      Страница открыта в браузере Minis. Закрыли её?");
        println!("      Нажмите Enter, чтобы открыть её снова.");
        println!("      Чтобы отвечать здесь, остановите siboot сочетанием Ctrl-C");
        println!("      и запустите его с параметром --terminal.");
        let url = url.to_string();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                if line.is_err() {
                    break;
                }
                web::show(&url);
            }
        });
    } else {
        println!("      Откройте адрес в браузере этого iPhone и");
        println!("      не закрывайте терминал:");
        println!("      {url}");
    }
}

fn init_logging(verbose: bool) {
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::prelude::*;

    let level = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::WARN
    };
    let quiet = if verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::ERROR
    };
    let filter = Targets::new()
        .with_default(level)
        .with_target("hyper_util", quiet);
    if ui::is_live() {
        let fmt = tracing_subscriber::fmt::layer()
            .with_writer(ui::LogLines)
            .with_ansi(false)
            .with_target(false)
            .without_time()
            .with_filter(filter);
        let _ = tracing_subscriber::registry()
            .with(fmt)
            .with(record_layer())
            .try_init();
    } else {
        let fmt = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(verbose)
            .without_time()
            .with_filter(filter);
        let _ = tracing_subscriber::registry()
            .with(fmt)
            .with(record_layer())
            .try_init();
    }
}

fn record_layer<S>() -> impl tracing_subscriber::Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::filter::{LevelFilter, Targets};
    use tracing_subscriber::Layer as _;

    let kept = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("hyper_util", LevelFilter::ERROR);
    tracing_subscriber::fmt::layer()
        .with_writer(ui::RecordLines)
        .with_ansi(false)
        .with_target(true)
        .without_time()
        .with_filter(kept)
}

fn watch_interrupt() {
    let _ = std::thread::Builder::new()
        .name("siboot-interrupt".into())
        .spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                if tokio::signal::ctrl_c().await.is_ok() {
                    ui::stop_now("Остановлено по Ctrl-C.");
                }
            });
        });
}

fn start_gate(custom_ipa: bool) -> bool {
    ui::get_ready_and_confirm(
        if custom_ipa {
            "Установка приложения"
        } else {
            "Установка SideInstaller"
        },
        &["Включите LocalDevVPN, статус должен быть Connected."],
        &["Затем нажмите «Начать установку»."],
        "Начать установку",
    )
}

fn retryable(f: &Fail) -> bool {
    !f.by_user && !f.known
}

fn retry_device(f: &Fail) -> bool {
    ui::pause_stage();
    let go = ui::get_ready_and_confirm(&f.advice, &[], &[], "Повторить");
    ui::resume_stage();
    go
}

async fn retry_network(args: &Args, f: &Fail) -> bool {
    ui::pause_stage();
    let go = vpn_handoff(
        args,
        &f.advice,
        &[],
        &["Переключите VPN, вернитесь в Minis и нажмите «Повторить»."],
        "Повторить",
    )
    .await;
    ui::resume_stage();
    go
}

async fn check_device(args: &Args) -> Result<preflight::Checked> {
    let mut first = true;
    loop {
        let outcome = if first {
            preflight::run(args).await
        } else {
            preflight::reach_device(args).await
        };
        first = false;
        match outcome {
            Ok(checked) => return Ok(checked),
            Err(f) => {
                if !retryable(&f) || !retry_device(&f) {
                    return Err(f);
                }
            }
        }
    }
}

async fn check_network(args: &Args) -> Result<()> {
    loop {
        match preflight::check_online(args).await {
            Ok(()) => return Ok(()),
            Err(f) => {
                if !retryable(&f) || !retry_network(args, &f).await {
                    return Err(f);
                }
            }
        }
    }
}

async fn open_tunnel(args: &Args, pairing_path: &Path) -> Result<tunnel::Connection> {
    loop {
        match tunnel::open(args.device, pairing_path).await {
            Ok(connection) => return Ok(connection),
            Err(f) => {
                if !retryable(&f) || !retry_device(&f) {
                    return Err(f);
                }
            }
        }
    }
}

async fn download_ipa(args: &Args) -> Result<PathBuf> {
    let downloads = args
        .ipa
        .as_deref()
        .is_none_or(|ipa| ipa.starts_with("http"));
    loop {
        match sign::fetch_ipa(args).await {
            Ok(path) => return Ok(path),
            Err(f) => {
                if !downloads || !retryable(&f) || !retry_network(args, &f).await {
                    return Err(f);
                }
            }
        }
    }
}

async fn pipeline(args: &Args) -> Result<()> {
    std::fs::create_dir_all(&args.home).map_err(|e| {
        Fail::new(
            format!("Не удалось создать {}.", args.home.display()),
            e.to_string(),
        )
    })?;

    let devmode = args.command == Command::DeveloperMode;
    match args.command {
        Command::All if args.ipa.is_some() => ui::plan(&APP_STEP_NAMES, APP_STEP_NAMES.len()),
        Command::All => ui::plan(&STEP_NAMES, STAGES),
        Command::Check => ui::plan(&CHECK_NAMES, CHECK_NAMES.len()),
        Command::Connect => ui::plan(&STEP_NAMES[..3], 3),
        Command::DeveloperMode => {
            let mut names = STEP_NAMES[..3].to_vec();
            names.push(DEVMODE_NAME);
            ui::plan(&names, DEVMODE_STAGES);
        }
    }
    let note = |f: Fail| {
        if devmode {
            devmode::unreachable_note(f)
        } else {
            f
        }
    };

    if args.command == Command::All && !start_gate(args.ipa.is_some()) {
        return Err(Fail::by_user("Остановлено до начала установки."));
    }

    ui::step(1, "Проверка LocalDevVPN и iPhone");
    let checked = check_device(args).await.map_err(note)?;
    ui::done("iPhone отвечает");

    if args.command == Command::Check {
        ui::step(2, "Проверка GitHub и сервера входа Apple");
        check_network(args).await?;
        ui::done("интернет доступен");
        ui::finish(
            "Всё готово",
            &["Запустите siboot, чтобы установить SideInstaller."],
        );
        return Ok(());
    }
    let args = &Args {
        device: checked.device,
        ..args.clone()
    };

    ui::step(2, "Сопряжение iPhone с siboot");
    let pairing_path = args.pairing_path();
    let source = pairing::ensure(args, &pairing_path).await.map_err(note)?;
    ui::done(match source {
        pairing::Source::Saved => "сохранённое сопряжение работает",
        pairing::Source::Rootshell => "получено из rootshell",
        pairing::Source::Minis => "сопряжено в Minis",
    });

    ui::step(3, "Подключение к iPhone через LocalDevVPN");
    let mut connection = open_tunnel(args, &pairing_path).await.map_err(note)?;
    let device = connection.describe().await.map_err(note)?;
    ui::ok(&format!(
        "{} — {}, iOS {}",
        device.name, device.product_type, device.ios_version
    ));
    ui::detail(&format!("UDID: {}", device.udid));
    ui::done(&format!("{} · iOS {}", device.name, device.ios_version));
    if args.command == Command::Connect {
        ui::finish(
            "Туннель работает",
            &["iPhone принимает файл сопряжения, туннель открыт."],
        );
        return Ok(());
    }
    if devmode {
        ui::step(4, "Запрос режима разработчика на iPhone");
        return developer_mode(args, &mut connection).await;
    }
    drop(connection);

    ui::step(4, "Переключение VPN и проверка интернета");
    if !vpn_handoff(
        args,
        "Настройте интернет для входа в Apple ID",
        &[
            "Отключите LocalDevVPN, если он мешает обычному интернету.",
            "При необходимости включите обычный VPN для Apple и GitHub.",
        ],
        &HANDOFF_NOTES,
        "Продолжить",
    )
    .await
    {
        return Err(Fail::by_user("Остановлено до входа в Apple ID."));
    }
    check_network(args).await?;
    ui::done("интернет доступен, GitHub и сервер Apple отвечают");

    ui::step(5, "Вход в учётную запись Apple");
    let mut session = account::sign_in(args).await?;
    ui::done(&session.team);

    ui::step(
        6,
        if args.ipa.is_some() {
            "Подпись приложения для этого iPhone"
        } else {
            "Подпись SideInstaller для этого iPhone"
        },
    );
    let ipa = download_ipa(args).await?;
    let signed = sign::sign(&mut session, &ipa, &device).await?;
    ui::done(&match &signed.version {
        Some(version) => format!("{} {version}: подписано", signed.name),
        None => format!("{}: подписано", signed.name),
    });

    if !vpn_handoff(
        args,
        "Подготовьте повторное подключение к iPhone",
        &[
            "Отключите обычный VPN, если он включён.",
            "Снова включите LocalDevVPN.",
        ],
        &HANDOFF_NOTES,
        "Продолжить",
    )
    .await
    {
        return Err(Fail::by_user(if args.ipa.is_some() {
            "Остановлено до установки приложения."
        } else {
            "Остановлено до установки SideInstaller."
        }));
    }

    ui::step(
        7,
        if args.ipa.is_some() {
            "Установка приложения на iPhone"
        } else {
            "Копирование файлов на iPhone"
        },
    );
    let mut connection = open_tunnel(args, &pairing_path).await?;
    let uploaded = install::install(&mut connection, &signed.app_dir).await?;
    ui::ok(&format!(
        "{} установлено ({})",
        signed.name, signed.bundle_id
    ));
    ui::done(&format!("установлено · {uploaded}"));

    if args.ipa.is_some() {
        ui::finish(
            &format!("{} установлено", signed.name),
            &[
                "Откройте приложение с главного экрана. При первом запуске подтвердите доверие к учётной записи Apple: Настройки › Основные › VPN и управление устройством.",
                team::lasts(session.kind, session.first),
            ],
        );
        return Ok(());
    }

    ui::step(8, "Передача сопряжения в SideInstaller");
    seed::hand_over(&mut connection, &signed.bundle_id, &pairing_path).await?;
    ui::done("SideInstaller получил файл сопряжения");

    ui::link("Открыть SideInstaller", "sideinstaller://");
    ui::finish(
        "SideInstaller установлен",
        &[
            "Откройте приложение с главного экрана. При первом запуске подтвердите доверие к учётной записи Apple: Настройки › Основные › VPN и управление устройством.",
            team::lasts(session.kind, session.first),
        ],
    );
    Ok(())
}

async fn vpn_handoff(
    args: &Args,
    title: &str,
    steps: &[&str],
    notes: &[&str],
    button: &str,
) -> bool {
    let bridge = Bridge::from_env();
    let keepalive = match KeepAlive::start(&bridge, &args.home, keepalive::PAIRING_SECONDS).await {
        Ok(player) => Some(player),
        Err(error) => {
            ui::warn(&format!(
                "Не удалось запустить фоновый плеер; переключать VPN небезопасно: {error}"
            ));
            return false;
        }
    };
    let confirmed = ui::get_ready_and_confirm(title, steps, notes, button);
    if let Some(player) = keepalive {
        player.stop().await;
    }
    confirmed
}

async fn developer_mode(args: &Args, connection: &mut tunnel::Connection) -> Result<()> {
    let request = if args.turn_on {
        devmode::Request::TurnOn
    } else {
        devmode::Request::Show
    };
    let confirm = || ui::confirm("включить режим сейчас?", false);
    let outcome = devmode::run(connection, request, confirm).await?;
    let unlock = "Разблокируйте iPhone и нажмите «Включить»";
    match outcome {
        devmode::Outcome::AlreadyOn => {
            ui::done("уже включён");
            ui::finish(
                "Режим разработчика уже включён",
                &["Запросы не отправлялись."],
            );
        }
        devmode::Outcome::Shown => {
            ui::done("переключатель появился в Настройках");
            ui::finish(
                "Режим разработчика доступен в Настройках",
                &[
                    "Переключатель находится внизу раздела Настройки › Конфиденциальность и безопасность. Включите его; iPhone перезагрузится.",
                    unlock,
                ],
            );
        }
        devmode::Outcome::ShownForPasscode => {
            ui::done("переключатель появился в Настройках");
            ui::finish(
                "Режим разработчика доступен в Настройках",
                &[
                    "iOS позволяет включить режим таким способом только без код-пароля, поэтому siboot показал переключатель в разделе Настройки › Конфиденциальность и безопасность. Включите его там; iPhone перезагрузится.",
                    unlock,
                ],
            );
        }
        devmode::Outcome::TurningOn => {
            ui::done("включение запрошено");
            ui::finish(
                "Включение режима разработчика",
                &["iPhone сейчас перезагрузится, Minis остановится.", unlock],
            );
        }
        devmode::Outcome::HungUp => {
            ui::done("iPhone отключился и перезагружается");
            ui::finish(
                "iPhone перезагружается",
                &[
                    "Соединение прервалось сразу после запроса — так происходит при перезагрузке для включения режима разработчика.",
                    unlock,
                    "Если iPhone не перезагрузится, снова запустите siboot developer-mode.",
                ],
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Args {
        match parse_args(line.split_whitespace().map(String::from)).unwrap() {
            Parsed::Run(args) => args,
            _ => panic!("not a run"),
        }
    }

    #[test]
    fn defaults_run_everything_through_localdevvpn() {
        let args = parse("");
        assert_eq!(args.command, Command::All);
        assert_eq!(args.device.to_string(), "10.7.0.1:49152");
        assert!(args
            .pairing_path()
            .ends_with(".siboot/rp_pairing_file.plist"));
    }

    #[test]
    fn commands_and_options_parse() {
        let args = parse("connect --device-ip 192.168.1.20 --port 50000 -v");
        assert_eq!(args.command, Command::Connect);
        assert_eq!(args.device.to_string(), "192.168.1.20:50000");
        assert!(args.verbose);
        assert_eq!(parse("check").command, Command::Check);
        assert!(parse("--plain").plain);
        assert!(!parse("").plain);
        assert_eq!(parse("--web").web, Some(true));
        assert_eq!(parse("developer-mode --web").web, Some(true));
        assert_eq!(parse("--terminal").web, Some(false));
        assert_eq!(parse("--plain").web, None);
        assert_eq!(parse("").web, None);
    }

    #[test]
    fn only_real_failures_offer_a_retry() {
        assert!(retryable(&Fail::new("Нет связи с iPhone.", "timeout")));
        assert!(!retryable(&Fail::by_user(
            "Остановлено до входа в Apple ID."
        )));
        assert!(!retryable(&Fail::known(preflight::OLD_MINIS)));
    }

    #[test]
    fn developer_mode_parses_with_or_without_turn_on() {
        let args = parse("developer-mode");
        assert_eq!(args.command, Command::DeveloperMode);
        assert!(!args.turn_on);
        assert!(parse("developer-mode --turn-on").turn_on);
        assert!(parse("--turn-on developer-mode -v").turn_on);
    }

    #[test]
    fn bad_input_is_refused() {
        for line in [
            "--device-ip",
            "--device-ip nope",
            "pair",
            "check connect",
            "developer-mode check",
            "--turn-on",
            "connect --turn-on",
            "--repair",
            "--advertiser minis",
        ] {
            assert!(
                parse_args(line.split_whitespace().map(String::from)).is_err(),
                "{line}"
            );
        }
    }
}
