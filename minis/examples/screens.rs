#![allow(dead_code)]

#[path = "../src/saved.rs"]
mod saved;
#[path = "../src/team.rs"]
mod team;
#[path = "../src/ui.rs"]
mod ui;
#[path = "../src/web.rs"]
mod web;

use std::thread::sleep;
use std::time::Duration;

fn wait(ms: u64) {
    sleep(Duration::from_millis(ms));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let page = if has("--web") {
        Some(web::start().expect("порт локального веб-сервера"))
    } else {
        None
    };
    ui::init(false, has("--plain") || page.is_some());
    if let Some(url) = &page {
        web::on_stop(|| ui::stop_now("Остановлено со страницы."));
        println!("Страница: {url}");
        if has("--open") {
            let _ = std::process::Command::new("open").arg(url).status();
        }
        web::wait_for_page(Duration::from_secs(600));
    }
    let devmode = has("developer-mode");
    let stop = has("stop");
    let port = if has("moved") {
        Port::Moved
    } else if has("nowhere") {
        Port::Nowhere
    } else {
        Port::Usual
    };
    let remembered = match (has("saved"), has("run-out")) {
        (true, false) => Some(Remembered::Lasting),
        (true, true) => Some(Remembered::RunOut),
        _ => None,
    };
    let code = if has("old-minis") {
        old_minis();
        1
    } else if run(
        devmode,
        stop,
        has("rootshell"),
        has("teams"),
        port,
        remembered,
    ) {
        0
    } else {
        1
    };
    if (stop || port == Port::Nowhere) && ui::confirm("скопировать журнал в буфер обмена?", true)
    {
        let log = ui::recorded().join("\n");
        let copied = std::process::Command::new("pbcopy")
            .env("LANG", "en_US.UTF-8")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write as _;
                child.stdin.take().map(|mut i| i.write_all(log.as_bytes()));
                child.wait()
            });
        match copied {
            Ok(status) if status.success() => ui::ok(&format!(
                "Журнал скопирован (строк: {}). Вставьте его в обращение за помощью.",
                log.lines().count()
            )),
            _ => ui::warn("Не удалось скопировать журнал: команда pbcopy недоступна."),
        }
        ui::note("Копия также сохранена в ~/.siboot/siboot.log.");
    }
    ui::close();
    std::process::exit(code);
}

fn old_minis() {
    ui::header("0.21.0", "Устанавливает SideInstaller на этот iPhone.");
    ui::plan(
        &[
            "Связь с iPhone",
            "Файл сопряжения",
            "Туннель к iPhone",
            "Вход в Apple ID",
            "Подпись SideInstaller",
            "Копирование на iPhone",
            "Настройка SideInstaller",
        ],
        7,
    );
    ui::step(1, "Проверка LocalDevVPN и iPhone");
    ui::activity("проверка Open Minis");
    wait(400);
    ui::link(
        "Обновить Open Minis",
        "itms-apps://apps.apple.com/app/id6759188481",
    );
    ui::failure(
        "Эта версия Open Minis старее 1.16. Для сопряжения iPhone нужна версия 1.16 или новее. Обновите Open Minis в App Store, откройте приложение и снова запустите siboot.",
        "",
    );
}

#[derive(Clone, Copy, PartialEq)]
enum Port {
    Usual,
    Moved,
    Nowhere,
}

fn reach(port: Port) -> bool {
    ui::activity("подключение к iPhone по адресу 10.7.0.1:49152");
    wait(500);
    if port == Port::Usual {
        ui::ok("iPhone отвечает по адресу 10.7.0.1:49152 (RPPairing, версия протокола 26)");
        return true;
    }
    ui::detail(
        "служба сопряжения по адресу 10.7.0.1:49152 не найдена: соединение отклонено (os error 111) (ConnectionRefused)",
    );
    ui::status("поиск службы сопряжения iPhone на других портах");
    let last = if port == Port::Moved { 35 } else { 100 };
    for percent in 0..=last {
        ui::percent("проверено портов", percent);
        wait(40);
    }
    ui::progress_end();
    if port == Port::Nowhere {
        ui::failure(
            "iPhone отклонил соединение по адресу 10.7.0.1:49152; на других портах служба сопряжения не найдена. Включите режим разработчика (Настройки › Конфиденциальность и безопасность › Режим разработчика) и проверьте Wi-Fi. Если они включены, перезагрузите iPhone, подключите LocalDevVPN и снова запустите siboot.",
            "Connection refused (os error 111) (ConnectionRefused); no other port answered as it \
             (16382 knocked on in 4.1 s; took the connection: 54666)",
        );
        return false;
    }
    ui::detail("служба найдена на порту 55021 (проверено 5869 из 16382 портов, время: 1,4 с)");
    ui::ok("iPhone отвечает по адресу 10.7.0.1:55021 (RPPairing, версия протокола 26)");
    true
}

fn run(
    devmode: bool,
    stop: bool,
    rootshell: bool,
    teams: bool,
    port: Port,
    remembered: Option<Remembered>,
) -> bool {
    let steps = [
        "Связь с iPhone",
        "Файл сопряжения",
        "Туннель к iPhone",
        "Вход в Apple ID",
        "Подпись SideInstaller",
        "Копирование на iPhone",
        "Настройка SideInstaller",
    ];
    if devmode {
        ui::header("0.21.0", "Показывает режим разработчика или включает его.");
        ui::plan(
            &[
                "Связь с iPhone",
                "Файл сопряжения",
                "Туннель к iPhone",
                "Режим разработчика",
            ],
            4,
        );
    } else {
        ui::header("0.21.0", "Устанавливает SideInstaller на этот iPhone.");
        ui::plan(&steps, 7);
    }

    ui::step(1, "Проверка LocalDevVPN и iPhone");
    if !reach(port) {
        return false;
    }
    ui::done("iPhone отвечает");

    ui::step(2, "Сопряжение iPhone с siboot");
    ui::activity("проверка возможности сопряжения через Open Minis");
    wait(500);
    if rootshell {
        if !take_from_rootshell() {
            return false;
        }
    } else if !pair_here() {
        return false;
    }
    ui::step(3, "Подключение к iPhone через LocalDevVPN");
    for (what, took) in [
        ("установление связи с iPhone", 300),
        ("проверка файла сопряжения", 500),
        ("запрос туннельного порта у iPhone", 600),
        ("запуск защищённого туннеля", 1400),
        ("получение списка служб iPhone", 900),
        ("запрос сведений об iPhone у lockdownd", 500),
    ] {
        ui::activity(what);
        wait(took);
        if stop && what == "starting the encrypted tunnel" {
            ui::failure(
                "Истекло время ожидания запуска защищённого туннеля.",
                "no answer within 20 s",
            );
            return false;
        }
    }
    ui::ok("iPhone Frizzle — iPhone18,1, iOS 27.1");
    ui::done("iPhone Frizzle · iOS 27.1");

    if devmode {
        ui::step(4, "Запрос режима разработчика на iPhone");
        ui::activity("проверка режима разработчика");
        wait(700);
        ui::status("Режим разработчика выключен");
        ui::warn(
            "Без код-пароля iPhone сразу перезагрузится для включения режима разработчика, а Minis остановится. После разблокировки нажмите «Включить» в уведомлении iOS.",
        );
        ui::note("Если установлен код-пароль, iOS откажет, и siboot вместо этого покажет переключатель в Настройках.");
        if !ui::confirm("включить режим сейчас?", false) {
            ui::failure(
                "Запрос не отправлен, режим разработчика остался выключен. Команда `siboot developer-mode` без --turn-on только показывает переключатель и не перезагружает iPhone.",
                "",
            );
            return false;
        }
        ui::activity("подключение к AMFI на iPhone");
        wait(800);
        ui::done("включение запрошено");
        ui::finish(
            "Включение режима разработчика",
            &[
                "iPhone сейчас перезагрузится, Minis остановится.",
                "Разблокируйте iPhone и нажмите «Включить».",
            ],
        );
        return true;
    }

    if !vpn_handoff(
        "Настройте интернет для входа в Apple ID",
        &[
            "Отключите LocalDevVPN, если он мешает обычному интернету.",
            "При необходимости включите обычный VPN для Apple и GitHub.",
        ],
        "Продолжить ко входу в Apple ID",
    ) {
        return false;
    }
    for (what, took, answer) in [
        (
            "проверка доступности GitHub",
            700,
            "GitHub доступен, релиз SideInstaller найден",
        ),
        (
            "проверка сервера входа Apple",
            600,
            "сервер входа Apple доступен",
        ),
    ] {
        ui::activity(what);
        wait(took);
        ui::ok(answer);
    }

    ui::step(4, "Вход в учётную запись Apple");
    let picked = match remembered {
        Some(_) => saved_account(),
        None => ask_email().map(|_| Picked::Typed),
    };
    let Some(picked) = picked else {
        ui::failure("Учётная запись Apple не указана.", "");
        return false;
    };
    let with_password = match picked {
        Picked::Saved if remembered == Some(Remembered::Lasting) => false,
        Picked::Saved => {
            ui::note(saved::RUN_OUT);
            true
        }
        Picked::Typed => true,
    };
    if with_password {
        if !password_and_code() {
            return false;
        }
    } else {
        ui::status("вход через new.ani.sidestore.io");
        wait(1400);
        ui::detail("использован сохранённый вход: он действует ещё 341 дн. 7 ч");
    }
    let (name, id, kind, first) = if teams {
        let listed = [
            ("Frizzle", "ABCDE12345", team::Kind::Free),
            ("Frizzle", "FGHIJ67890", team::Kind::Paid),
        ];
        ui::status("получение списка команд разработчиков");
        wait(700);
        let labels: Vec<String> = listed
            .iter()
            .map(|(name, id, kind)| team::label(Some(name), id, *kind))
            .collect();
        ui::status(&team::found(listed.len()));
        ui::note(team::NOTE);
        let Some(i) = ui::choose(team::QUESTION, &labels) else {
            ui::failure("Команда разработчиков не выбрана.", "");
            return false;
        };
        let (name, id, kind) = listed[i];
        (name, id, kind, i == 0)
    } else {
        ("Frizzle", "ABCDE12345", team::Kind::Unknown, true)
    };
    let summary = team::summary(Some(name), id, kind);
    ui::ok(&if with_password {
        format!("вход выполнен — {summary}")
    } else {
        format!("вход выполнен без пароля — {summary}")
    });
    if picked == Picked::Typed {
        ui::note(saved::SAVED);
    }
    ui::done(&summary);
    sign_and_install(id, kind, first)
}

fn ask_email() -> Option<String> {
    ui::ask_text(
        "войдите в учётную запись Apple",
        "Электронная почта",
        "адрес учётной записи Apple",
        "Электронная почта Apple ID: ",
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Remembered {
    Lasting,
    RunOut,
}

#[derive(Clone, Copy, PartialEq)]
enum Picked {
    Saved,
    Typed,
}

fn saved_account() -> Option<Picked> {
    let mut accounts = vec![
        "frizzle@icloud.com".to_string(),
        "frizzle.work@example.com".to_string(),
    ];
    loop {
        if accounts.is_empty() {
            return ask_email().map(|_| Picked::Typed);
        }
        let index = ui::choose(saved::QUESTION, &saved::options(&accounts))?;
        match saved::picked(index, accounts.len()) {
            saved::Pick::Account(_) => return Some(Picked::Saved),
            saved::Pick::Another => {
                if ask_email().is_some() {
                    return Some(Picked::Typed);
                }
            }
            saved::Pick::Forget => {
                let index = match accounts.as_slice() {
                    [_] => Some(0),
                    _ => ui::choose(saved::FORGET_WHICH, &accounts),
                };
                if let Some(i) = index {
                    if ui::confirm(&format!("забыть {}?", accounts[i]), false) {
                        let email = accounts.remove(i);
                        ui::ok(&format!("Удалена запись {email} и сохранённый вход"));
                    }
                }
            }
        }
    }
}

fn password_and_code() -> bool {
    let password = ui::ask_secret(
        "войдите в учётную запись Apple",
        "Пароль",
        "он будет отправлен только в Apple",
        "Пароль (не отображается; отправляется только в Apple): ",
    );
    if !matches!(password, Ok(Some(ref p)) if !p.is_empty()) {
        ui::failure("Пароль не указан.", "");
        return false;
    }
    ui::status("вход через new.ani.sidestore.io");
    wait(1500);
    ui::warn(
        "Сервер new.ani.sidestore.io не сработал: anisette-сервер ответил 503 Service Unavailable",
    );
    ui::status("вход через mac-ani.jkcoxson.com");
    wait(1200);
    let mut hints = vec![ui::Hint::Plain(
        "код отправлен на ваши устройства Apple".into(),
    )];
    loop {
        match ui::ask_code_or_more("введите код от Apple", "Код", &hints) {
            Some(ui::CodeAnswer::Code(code)) if code == "123456" => break,
            Some(ui::CodeAnswer::Code(_)) => {
                hints.retain(|hint| !matches!(hint, ui::Hint::Warning(_)));
                hints.insert(
                    0,
                    ui::Hint::Warning("Ответ Apple: неверный код проверки.".into()),
                );
            }
            Some(ui::CodeAnswer::More) => {
                let ways = [
                    "Отправить SMS на +1 (•••) •••-••42".to_string(),
                    "Позвонить на +1 (•••) •••-••42".to_string(),
                    "Отправить на устройства Apple".to_string(),
                    "Отправить ещё раз".to_string(),
                    "Отменить вход".to_string(),
                ];
                match ui::choose("как Apple отправит код?", &ways) {
                    Some(4) => {
                        ui::failure("Вход отменён.", "");
                        return false;
                    }
                    Some(i) => {
                        hints = vec![ui::Hint::Plain(match i {
                            0 => "отправлено по SMS на +1 (•••) •••-••42".into(),
                            1 => "Apple звонит на +1 (•••) •••-••42".into(),
                            _ => "отправлено на ваши устройства Apple".into(),
                        })];
                        ui::status("запрос нового кода у Apple");
                        wait(900);
                    }
                    None => {}
                }
            }
            Some(ui::CodeAnswer::Cancel) => {
                ui::failure("Вход отменён.", "");
                return false;
            }
            None => match ui::ask("Код: ") {
                Some(code) if code == "123456" => break,
                Some(_) => {}
                None => {
                    ui::failure("Вход отменён.", "");
                    return false;
                }
            },
        }
        ui::status("проверка кода в Apple");
        wait(800);
    }
    ui::status("проверка кода в Apple");
    wait(900);
    true
}

fn sign_and_install(id: &str, kind: team::Kind, first: bool) -> bool {
    ui::step(5, "Подпись SideInstaller для этого iPhone");
    ui::status("загрузка SideInstaller");
    let total: u64 = 24 * 1_048_576 + 400_000;
    for i in 0..=60u64 {
        ui::progress("загружено", total * i / 60, total);
        wait(45);
    }
    ui::progress_end();
    ui::ok("SideInstaller загружен (24,4 МБ)");
    ui::status("подписание (это может занять некоторое время)");
    wait(4200);
    ui::ok(&format!(
        "SideInstaller подписан, идентификатор: com.frizzlem.sideinstaller.{id}"
    ));
    ui::done("SideInstaller 1.7.0 подписан");

    if !vpn_handoff(
        "Подготовьтесь снова подключиться к iPhone",
        &[
            "Отключите обычный VPN, если он включён.",
            "Снова включите LocalDevVPN.",
        ],
        "Продолжить установку",
    ) {
        ui::failure("Остановлено до установки SideInstaller.", "");
        return false;
    }

    ui::step(6, "Установка SideInstaller");
    let total: u64 = 51 * 1_048_576;
    ui::status("передача 412 файлов (51,0 МБ)");
    for i in 0..=80u64 {
        ui::progress("передано", total * i / 80, total);
        wait(50);
    }
    ui::progress_end();
    ui::status("проверка целостности файлов");
    wait(700);
    ui::status("установка");
    for percent in (0..=100).step_by(4) {
        ui::percent("установлено", percent);
        wait(70);
    }
    ui::progress_end();
    ui::ok(&format!(
        "SideInstaller установлен (com.frizzlem.sideinstaller.{id})"
    ));
    ui::done("установлено · 51,0 МБ, 412 файлов");

    ui::step(7, "Передача сопряжения в SideInstaller");
    ui::activity("запись файла сопряжения");
    wait(900);
    ui::done("SideInstaller получил файл сопряжения");

    ui::link("Открыть SideInstaller", "sideinstaller://");
    ui::finish(
        "SideInstaller установлен",
        &[
            "Откройте приложение с главного экрана. При первом запуске подтвердите доверие к учётной записи Apple: Настройки › Основные › VPN и управление устройством.",
            team::lasts(kind, first),
        ],
    );
    true
}

fn vpn_handoff(title: &str, steps: &[&str], button: &str) -> bool {
    ui::get_ready_and_confirm(
        title,
        steps,
        &[
            "Плеер с тишиной не даёт iOS приостановить Minis в Настройках. Не закрывайте его.",
            "Вернитесь в Minis, закройте окно плеера, если оно перекрывает экран, и подтвердите продолжение.",
            "LocalDevVPN нужен только для подключения к iPhone. siboot не может переключать или определять VPN.",
        ],
        button,
    )
}

fn pair_here() -> bool {
    ui::ok("Open Minis может выполнить сопряжение с iPhone");
    ui::status("начните сопряжение, когда будете готовы");
    if !ui::get_ready(
        "Сопряжение iPhone с siboot",
        &[
            "Откройте Настройки › Конфиденциальность и безопасность › Режим разработчика и прокрутите вниз.",
            "Нажмите Pair with siboot. Пункт может появиться через несколько секунд.",
            "Введите код-пароль iPhone, затем шестизначный код siboot из уведомления.",
            "Когда Настройки сообщат о сопряжении, вернитесь в Minis.",
        ],
        &["После начала поверх Minis откроется плеер с тишиной. Он не даст iOS приостановить Minis в Настройках — не закрывайте его. siboot остановит плеер после возвращения."],
        "Начать сопряжение",
    ) {
        ui::failure("Остановлено до начала сопряжения.", "");
        return false;
    }
    ui::ok("Pair with siboot отображается в Настройках");
    ui::status("ожидание Pair with siboot в Настройках (до 10 минут)");
    wait(2500);
    ui::status("Настройки подключились: ожидание запроса кода");
    wait(600);
    ui::status("ожидание ввода кода в Настройках");
    ui::show_code(
        "975088",
        "Введите код в Настройках. Он также отправлен в уведомлении.",
    );
    wait(5000);
    ui::code_gone();
    ui::status("сопряжение выполнено: вернитесь в Minis, чтобы продолжить");
    wait(1500);
    ui::ok("сопряжение с iPhone Frizzle выполнено (iPhone18,1)");
    ui::activity("проверка нового сопряжения на iPhone");
    wait(700);
    ui::ok("iPhone принимает новое сопряжение");
    ui::done("сопряжение выполнено в Minis");
    true
}

fn take_from_rootshell() -> bool {
    ui::ok("rootshell предлагает файл сопряжения");
    let mut tries = 3;
    loop {
        let Some(code) = ui::ask_code(
            "rootshell предлагает файл сопряжения",
            "Код",
            "введите показанный код передачи",
            "Код передачи из rootshell: ",
        ) else {
            ui::failure("Остановлено на запросе кода передачи.", "");
            return false;
        };
        ui::activity("отправка кода в rootshell");
        wait(400);
        if code == "481516" {
            break;
        }
        tries -= 1;
        if tries == 0 {
            ui::failure("Код передачи не совпал. Запустите siboot снова и введите код, показанный rootshell.", "");
            return false;
        }
        ui::warn(&format!(
            "Это не код из rootshell (осталось попыток: {tries}).",
        ));
    }
    ui::ok("файл сопряжения получен из rootshell (1184 байт)");
    ui::activity("проверка файла на iPhone");
    wait(700);
    ui::done("получено из rootshell");
    true
}
