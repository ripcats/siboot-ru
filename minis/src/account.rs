use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use isideload::anisette::{
    remote_v3::RemoteV3AnisetteProvider, AnisetteDataGenerator, AnisetteProvider,
};
use isideload::auth::apple_account::{
    AppToken, AppleAccount, TwoFactorCallbackParams, TwoFactorCallbackResponse,
};
use isideload::auth::grandslam::GrandSlam;
use isideload::dev::certificates::DevelopmentCertificate;
use isideload::dev::developer_session::DeveloperSession;
use isideload::dev::device_type::{dev_url, DeveloperDeviceType};
use isideload::dev::teams::{DeveloperTeam, TeamsApi};
use isideload::sideload::builder::MaxCertsBehavior;
use isideload::sideload::sideloader::Sideloader;
use isideload::sideload::{SideloaderBuilder, TeamSelection};
use isideload::util::fs_storage::FsStorage;
use rootcause::Report;
use serde::Deserialize;
use tokio::sync::RwLock;

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(90);

use crate::apple_session;
use crate::error_text::report_text;
use crate::fail::{Fail, Result};
use crate::saved;
use crate::team::{self, Kind};
use crate::{ui, Args};

pub const SERVERS: &[&str] = &[
    POOL,
    "https://new.ani.sidestore.io",
    "https://mac-ani.jkcoxson.com",
    "https://ani.iloader.app",
    "https://ani3.stikstore.app",
    "https://ani.jkcoxson.com",
    "https://ani2.stikstore.app",
    "https://win.ani.sidestore.io",
];

const POOL: &str = "https://anisette.lillq.me";

const MACHINE_NAME: &str = "siboot";

pub struct Session {
    pub sideloader: Sideloader,
    pub team: String,
    pub kind: Kind,
    pub first: bool,
}

pub async fn sign_in(args: &Args) -> Result<Session> {
    let storage = args.home.join("isideload");
    std::fs::create_dir_all(&storage).map_err(|e| {
        Fail::new(
            format!("Не удалось создать {}.", storage.display()),
            e.to_string(),
        )
    })?;
    start_list(&args.home, &storage);
    let apple_id = match args
        .apple_id
        .clone()
        .or_else(|| std::env::var("SIBOOT_APPLE_ID").ok())
    {
        Some(id) => id,
        None => which_account(&args.home, &storage).unwrap_or_default(),
    };
    let typed = apple_id.trim().to_string();
    let apple_id = typed.to_lowercase();
    if apple_id.is_empty() {
        return Err(Fail::by_user("Учётная запись Apple не указана."));
    }
    adopt_folder_typed_as(&storage, &typed, &apple_id);

    let servers = candidates(args.anisette.as_deref(), last_server(&args.home).as_deref());
    let password = match std::env::var("SIBOOT_PASSWORD") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            match saved_token(&storage, &apple_id, unix_now()) {
                Saved::Usable(token) => match with_token(&token, &servers, &storage).await? {
                    ByToken::Opened(server, developer, teams) => {
                        let how = Signed::WithoutPassword;
                        return signed_in(
                            args, &storage, &apple_id, &server, developer, teams, how,
                        )
                        .await;
                    }
                    ByToken::Refused => ui::note(saved::RUN_OUT),
                },
                Saved::RunOut => ui::note(saved::RUN_OUT),
                Saved::None => {}
            }
            ask_password()?
        }
    };

    let mut last_error = String::new();
    for server in &servers {
        ui::status(&format!("вход через {}", host_of(server)));
        let result = tokio::time::timeout(
            SIGN_IN_TIMEOUT,
            apple_session::open(
                &apple_id, &password, server, &storage, true, two_factor, "Apple ID",
            ),
        )
        .await;
        let result = match result {
            Ok(result) => result,
            Err(_) => Err(format!(
                "Время ожидания входа Apple ID истекло через {} с",
                SIGN_IN_TIMEOUT.as_secs()
            )),
        };
        match result {
            Ok((developer, teams)) => {
                let how = Signed::WithPassword;
                return signed_in(args, &storage, &apple_id, server, developer, teams, how).await;
            }
            Err(e) => {
                if let Some(advice) = stops_sign_in(&e) {
                    return Err(stopped(advice, e));
                }
                ui::warn(&format!(
                    "Сервер {} не сработал: {}",
                    host_of(server),
                    first_line(&e)
                ));
                last_error = e;
            }
        }
    }
    Err(no_server(servers.len(), last_error))
}

#[derive(Clone, Copy)]
enum Signed {
    WithPassword,
    WithoutPassword,
}

async fn signed_in(
    args: &Args,
    storage: &Path,
    apple_id: &str,
    server: &str,
    mut developer: DeveloperSession,
    teams: Vec<DeveloperTeam>,
    how: Signed,
) -> Result<Session> {
    remember_server(&args.home, server);
    let new = remember_account(&args.home, apple_id);
    let (picked, kind, first) = pick_team(&mut developer, teams).await?;
    let team = team::summary(picked.name.as_deref(), &picked.team_id, kind);
    let mut sideloader = SideloaderBuilder::new(developer, apple_id.to_string())
        .team_selection(TeamSelection::First)
        .max_certs_behavior(MaxCertsBehavior::Prompt(Box::new(
            choose_certificates_to_revoke,
        )))
        .storage(Box::new(FsStorage::new(storage.to_path_buf())))
        .machine_name(MACHINE_NAME.to_string())
        .build();
    sideloader.set_team(picked);
    ui::ok(&match how {
        Signed::WithPassword => format!("вход выполнен — {team}"),
        Signed::WithoutPassword => format!("вход выполнен без пароля — {team}"),
    });
    if new {
        ui::note(saved::SAVED);
    }
    Ok(Session {
        sideloader,
        team,
        kind,
        first,
    })
}

fn stopped(advice: &'static str, error: String) -> Fail {
    let mut f = Fail::new(advice, error);
    f.by_user = advice == CANCELLED;
    f
}

fn no_server(servers: usize, last_error: String) -> Fail {
    Fail::new(
        format!("Не удалось войти через серверы anisette ({servers}). Попробуйте позже."),
        last_error,
    )
}

fn ask_email() -> Option<String> {
    ui::ask_text(
        "войдите в учётную запись Apple",
        "Электронная почта",
        "адрес учётной записи Apple",
        "Электронная почта Apple ID: ",
    )
}

fn ask_password() -> Result<String> {
    let password = ui::ask_secret(
        "войдите в учётную запись Apple",
        "Пароль",
        "он будет отправлен только в Apple",
        "Пароль (не отображается; отправляется только в Apple): ",
    )
    .map_err(|e| Fail::new("Не удалось прочитать пароль из терминала.", e.to_string()))?
    .unwrap_or_default();
    if password.is_empty() {
        return Err(Fail::by_user("Пароль не указан."));
    }
    Ok(password)
}

fn which_account(home: &Path, storage: &Path) -> Option<String> {
    loop {
        let accounts = saved::list(home);
        if accounts.is_empty() {
            return ask_email();
        }
        let index = ui::choose(saved::QUESTION, &saved::options(&accounts))?;
        match saved::picked(index, accounts.len()) {
            saved::Pick::Account(i) => return accounts.get(i).cloned(),
            saved::Pick::Another => {
                if let Some(email) = ask_email() {
                    return Some(email);
                }
            }
            saved::Pick::Forget => forget_one(home, storage, &accounts),
        }
    }
}

fn start_list(home: &Path, storage: &Path) {
    if saved::exists(home) {
        return;
    }
    if let Err(e) = saved::start(home, &signed_in_before(storage)) {
        ui::detail(&format!(
            "Не удалось сохранить список учётных записей Apple: {e}"
        ));
    }
}

fn signed_in_before(storage: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(storage) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let token = SavedToken::read(&entry.path().join(SESSION_FILE))?;
            (apple_session::account_dir(storage, &token.apple_id) == entry.path())
                .then_some((token.saved_at, token.apple_id))
        })
        .collect();
    found.sort_by_key(|(saved_at, _)| std::cmp::Reverse(*saved_at));
    found.into_iter().map(|(_, apple_id)| apple_id).collect()
}

fn remember_account(home: &Path, apple_id: &str) -> bool {
    match saved::remember(home, apple_id) {
        Ok(new) => new,
        Err(e) => {
            ui::detail(&format!(
                "Не удалось сохранить учётную запись Apple для следующего запуска: {e}"
            ));
            false
        }
    }
}

fn forget_one(home: &Path, storage: &Path, accounts: &[String]) {
    let index = match accounts {
        [_] => Some(0),
        _ => ui::choose(saved::FORGET_WHICH, accounts),
    };
    let Some(email) = index.and_then(|i| accounts.get(i)) else {
        return;
    };
    if !ui::confirm(&format!("забыть {email}?"), false) {
        return;
    }
    match saved::forget(home, email) {
        Ok(_) => {
            if apple_session::forget(storage, email) {
                ui::ok(&format!("Удалена запись {email} и сохранённый вход"));
            } else {
                ui::ok(&format!("Удалена запись {email}"));
            }
        }
        Err(e) => ui::warn(&format!("Не удалось удалить {email}: {e}")),
    }
}

const SESSION_FILE: &str = "developer_session.json";

const EXPIRY_MARGIN_SECS: u64 = 10 * 60;

#[derive(Deserialize)]
struct SavedToken {
    apple_id: String,
    adsid: String,
    token: String,
    duration: u64,
    expiry: u64,
    saved_at: u64,
}

impl SavedToken {
    fn read(path: &Path) -> Option<SavedToken> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    fn expires_at(&self) -> Option<u64> {
        apple_session::stated_expiry(self.expiry)
            .or_else(|| (self.duration > 0).then(|| self.saved_at.saturating_add(self.duration)))
    }

    fn app_token(&self) -> AppToken {
        AppToken {
            token: self.token.clone(),
            duration: self.duration,
            expiry: self.expiry,
        }
    }

    fn lifetime(&self, now: u64) -> String {
        match self.expires_at() {
            Some(at) if at > now => {
                let left = at - now;
                format!(
                    "Вход Apple действует ещё {} дн. {} ч",
                    left / 86_400,
                    left % 86_400 / 3_600
                )
            }
            Some(_) => "Срок действия входа Apple истёк".into(),
            None => "Apple не указала срок действия входа".into(),
        }
    }
}

enum Saved {
    Usable(SavedToken),
    RunOut,
    None,
}

fn saved_token(storage: &Path, apple_id: &str, now: u64) -> Saved {
    let path = apple_session::account_dir(storage, apple_id).join(SESSION_FILE);
    match SavedToken::read(&path) {
        Some(token) if token.apple_id != apple_id => Saved::None,
        Some(token)
            if token
                .expires_at()
                .is_some_and(|at| now.saturating_add(EXPIRY_MARGIN_SECS) >= at) =>
        {
            Saved::RunOut
        }
        Some(token) => Saved::Usable(token),
        None => Saved::None,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

enum ByToken {
    Opened(String, DeveloperSession, Vec<DeveloperTeam>),
    Refused,
}

async fn with_token(token: &SavedToken, servers: &[String], storage: &Path) -> Result<ByToken> {
    let mut last_error = String::new();
    for server in servers {
        ui::status(&format!("вход через {}", host_of(server)));
        match open_with_token(token, server, storage).await {
            Ok((developer, teams)) => {
                let lifetime = token.lifetime(unix_now());
                tracing::info!(
                    "Apple ID: вход по сохранённому токену, пароль не нужен ({lifetime})"
                );
                ui::detail(&format!("использован сохранённый вход: {lifetime}"));
                return Ok(ByToken::Opened(server.clone(), developer, teams));
            }
            Err(Tried::Answered(e)) if refuses_token(&e) => {
                tracing::info!(
                    "Apple ID: Apple больше не принимает сохранённый токен ({}); запрашивается пароль",
                    first_line(&e)
                );
                return Ok(ByToken::Refused);
            }
            Err(Tried::Answered(e) | Tried::NotReached(e)) => {
                if let Some(advice) = stops_sign_in(&e) {
                    return Err(stopped(advice, e));
                }
                ui::warn(&format!(
                    "Сервер {} не сработал: {}",
                    host_of(server),
                    first_line(&e)
                ));
                last_error = e;
            }
        }
    }
    Err(no_server(servers.len(), last_error))
}

enum Tried {
    NotReached(String),
    Answered(String),
}

async fn open_with_token(
    token: &SavedToken,
    server: &str,
    storage: &Path,
) -> std::result::Result<(DeveloperSession, Vec<DeveloperTeam>), Tried> {
    let anisette = || {
        RemoteV3AnisetteProvider::new(
            server,
            Box::new(FsStorage::new(storage.to_path_buf())),
            "0".to_string(),
        )
        .map_err(|e| Tried::NotReached(format!("anisette provider: {}", report_text(&e))))
    };

    let without_url_bag = async {
        let provider = anisette()?;
        let client_info = provider
            .get_client_info()
            .await
            .map_err(|e| Tried::NotReached(format!("anisette client info: {}", report_text(&e))))?;
        let client = GrandSlam::without_url_bag(client_info, false)
            .map_err(|e| Tried::NotReached(format!("GrandSlam client: {}", report_text(&e))))?;
        let mut developer = DeveloperSession::new(
            token.app_token(),
            token.adsid.clone(),
            Arc::new(client),
            AnisetteDataGenerator::new(Arc::new(RwLock::new(provider))),
        );
        let teams = developer
            .list_teams()
            .await
            .map_err(|e| Tried::Answered(report_text(&e)))?;
        Ok::<_, Tried>((developer, teams))
    };
    match without_url_bag.await {
        Ok(opened) => return Ok(opened),
        Err(Tried::NotReached(e) | Tried::Answered(e)) => tracing::info!(
            "Apple ID: не удалось сразу использовать сохранённый токен ({}); повторная попытка через полный вход",
            first_line(&e)
        ),
    }

    let account = AppleAccount::builder(&token.apple_id)
        .anisette_provider(anisette()?)
        .build()
        .await
        .map_err(|e| {
            Tried::NotReached(format!(
                "anisette client info or URL bag: {}",
                report_text(&e)
            ))
        })?;
    let mut developer = DeveloperSession::new(
        token.app_token(),
        token.adsid.clone(),
        account.grandslam_client.clone(),
        account.anisette_generator.clone(),
    );
    let teams = developer
        .list_teams()
        .await
        .map_err(|e| Tried::Answered(format!("developer session: {}", report_text(&e))))?;
    Ok((developer, teams))
}

fn refuses_token(error: &str) -> bool {
    let e = error.to_lowercase();
    let transient = [
        "anisette",
        "error sending request",
        "истекло время ожидания",
        "(429 ",
        "server error (5",
    ]
    .iter()
    .any(|s| e.contains(s));
    let account = e.contains("developer error 1102") || e.contains("developer error 2160");
    !transient && !account
}

async fn pick_team(
    developer: &mut DeveloperSession,
    mut teams: Vec<DeveloperTeam>,
) -> Result<(DeveloperTeam, Kind, bool)> {
    let none = "Вход выполнен, но Apple не сообщила команду разработчиков.";
    if teams.is_empty() {
        teams = developer
            .list_teams()
            .await
            .map_err(|e| Fail::new(none, crate::error_text::report_text(&e)))?;
    }
    if teams.len() < 2 {
        let only = teams
            .pop()
            .ok_or_else(|| Fail::new(none, "Команды разработчиков отсутствуют"))?;
        return Ok((only, Kind::Unknown, true));
    }
    ui::status("получение списка команд разработчиков");
    let known = match list_kinds(developer).await {
        Ok(known) => known,
        Err(e) => {
            ui::detail(&format!(
                "не удалось отличить бесплатные команды от платных: {}",
                first_line(&crate::error_text::report_text(&e))
            ));
            HashMap::new()
        }
    };
    let kinds: Vec<Kind> = teams
        .iter()
        .map(|t| known.get(&t.team_id).copied().unwrap_or(Kind::Unknown))
        .collect();
    let labels: Vec<String> = teams
        .iter()
        .zip(&kinds)
        .map(|(t, kind)| team::label(t.name.as_deref(), &t.team_id, *kind))
        .collect();
    ui::status(&team::found(teams.len()));
    ui::note(team::NOTE);
    match ui::choose(team::QUESTION, &labels) {
        Some(i) if i < teams.len() => Ok((teams.swap_remove(i), kinds[i], i == 0)),
        _ => Err(Fail::by_user("Команда разработчиков не выбрана.")),
    }
}

async fn list_kinds(
    developer: &mut DeveloperSession,
) -> std::result::Result<HashMap<String, Kind>, Report> {
    let listed: Vec<team::Listed> = developer
        .send_dev_request(
            &dev_url("listTeams", DeveloperDeviceType::Any),
            None,
            "teams",
        )
        .await?;
    Ok(team::kinds(&listed))
}

fn candidates(pick: Option<&str>, last: Option<&str>) -> Vec<String> {
    if let Some(pick) = pick {
        return vec![pick.trim().trim_end_matches('/').to_string()];
    }
    let trim = |s: &str| s.trim().trim_end_matches('/').to_string();
    let last = last.map(trim).filter(|s| !s.is_empty());
    let shares_identity = |s: &str| s != POOL && s != WINDOWS_SERVER;
    let mut order: Vec<String> = last.iter().cloned().collect();
    if last
        .as_deref()
        .is_some_and(|l| SERVERS.contains(&l) && shares_identity(l))
    {
        order.extend(
            SERVERS
                .iter()
                .filter(|s| shares_identity(**s))
                .map(|s| s.to_string()),
        );
    }
    order.extend(SERVERS.iter().map(|s| s.to_string()));
    let mut out: Vec<String> = Vec::new();
    for s in order {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

const WINDOWS_SERVER: &str = "https://win.ani.sidestore.io";

fn last_server_path(home: &Path) -> PathBuf {
    home.join("anisette-server")
}

fn last_server(home: &Path) -> Option<String> {
    std::fs::read_to_string(last_server_path(home))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn remember_server(home: &Path, server: &str) {
    let _ = std::fs::write(last_server_path(home), server);
}

const CANCELLED: &str = "Вход отменён.";

fn stops_sign_in(error: &str) -> Option<&'static str> {
    if ["-20101", "-20970", "-22406"]
        .iter()
        .any(|c| error.contains(c))
    {
        return Some(
            "Apple отклонила учётную запись или пароль. Проверьте их и снова запустите siboot. Если данные верны, Apple могла временно ограничить вход — подождите и повторите попытку.",
        );
    }
    if error.contains("429") || error.to_ascii_lowercase().contains("too many") {
        return Some(
            "Apple временно ограничила попытки входа. Подождите и снова запустите siboot.",
        );
    }
    if error.contains("No 2FA") {
        return Some(CANCELLED);
    }
    None
}

fn adopt_folder_typed_as(storage: &Path, typed: &str, apple_id: &str) {
    if typed == apple_id {
        return;
    }
    let old = apple_session::account_dir(storage, typed);
    let new = apple_session::account_dir(storage, apple_id);
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(old, new);
    }
}

fn host_of(url: &str) -> &str {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

fn two_factor(
    params: TwoFactorCallbackParams,
) -> impl std::future::Future<Output = std::result::Result<TwoFactorCallbackResponse, Report>> + Send
{
    let prompt = Prompt::from(&params);
    async move {
        let answer = tokio::task::spawn_blocking(move || ask_two_factor(&prompt))
            .await
            .unwrap_or(TwoFactorCallbackResponse::Abort);
        Ok(answer)
    }
}

struct Prompt {
    method: Method,
    selected: Option<u32>,
    last_error: Option<String>,
    numbers: Vec<(u32, String, String)>,
}

#[derive(PartialEq, Eq, Debug)]
enum Method {
    Devices,
    Sms,
    Voice,
    Choose,
}

impl From<&TwoFactorCallbackParams> for Prompt {
    fn from(p: &TwoFactorCallbackParams) -> Self {
        let method = if p.unknown {
            Method::Choose
        } else if p.voice {
            Method::Voice
        } else if p.sms {
            Method::Sms
        } else {
            Method::Devices
        };
        Self {
            method,
            selected: p.selected_number_id,
            last_error: p.last_error.clone(),
            numbers: p
                .numbers
                .iter()
                .map(|n| (n.id, n.number_with_dial_code.clone(), n.push_mode.clone()))
                .collect(),
        }
    }
}

fn ask_two_factor(prompt: &Prompt) -> TwoFactorCallbackResponse {
    if ui::interactive() {
        if let Some(answer) = ask_two_factor_live(prompt) {
            return answer;
        }
    }
    ask_two_factor_plain(prompt)
}

impl Prompt {
    fn number(&self, id: Option<u32>) -> String {
        self.numbers
            .iter()
            .find(|(n, _, _)| Some(*n) == id)
            .map(|(_, number, _)| number.clone())
            .unwrap_or_else(|| "your trusted number".into())
    }

    fn ways(&self) -> Vec<(String, TwoFactorCallbackResponse)> {
        let mut ways = Vec::new();
        for (id, number, mode) in &self.numbers {
            if mode != "voice" {
                ways.push((
                    format!("Отправить SMS на {number}"),
                    TwoFactorCallbackResponse::SendSms(*id),
                ));
            }
            ways.push((
                format!("Позвонить на {number}"),
                TwoFactorCallbackResponse::CallNumber(*id),
            ));
        }
        ways.push((
            "Отправить на устройства Apple".into(),
            TwoFactorCallbackResponse::SendToDevices,
        ));
        if self.method != Method::Choose {
            ways.push((
                "Отправить ещё раз".into(),
                TwoFactorCallbackResponse::ResendCode,
            ));
        }
        ways.push(("Отменить вход".into(), TwoFactorCallbackResponse::Abort));
        ways
    }
}

fn ask_two_factor_live(prompt: &Prompt) -> Option<TwoFactorCallbackResponse> {
    let sent = match prompt.method {
        Method::Devices => Some("отправлен на ваши устройства Apple".to_string()),
        Method::Sms => Some(format!(
            "отправлен по SMS на {}",
            prompt.number(prompt.selected)
        )),
        Method::Voice => Some(format!(
            "Apple звонит на номер {}",
            prompt.number(prompt.selected)
        )),
        Method::Choose => None,
    };
    let mut warning = prompt
        .last_error
        .as_ref()
        .map(|e| format!("Ответ Apple: {e}"));
    loop {
        if let Some(sent) = &sent {
            let mut hints = Vec::new();
            if let Some(warning) = warning.take() {
                hints.push(ui::Hint::Warning(warning));
            }
            hints.push(ui::Hint::Plain(sent.clone()));
            match ui::ask_code_or_more("введите код от Apple", "Код", &hints)? {
                ui::CodeAnswer::Code(code) => {
                    return Some(TwoFactorCallbackResponse::SubmitCode(code))
                }
                ui::CodeAnswer::Cancel => return Some(TwoFactorCallbackResponse::Abort),
                ui::CodeAnswer::More => {}
            }
        }
        let ways = prompt.ways();
        let labels: Vec<String> = ways.iter().map(|(label, _)| label.clone()).collect();
        match ui::choose("как Apple отправит код?", &labels) {
            Some(i) => return ways.into_iter().nth(i).map(|(_, answer)| answer),
            None if sent.is_some() => continue,
            None => return Some(TwoFactorCallbackResponse::Abort),
        }
    }
}

fn ask_two_factor_plain(prompt: &Prompt) -> TwoFactorCallbackResponse {
    println!();
    if let Some(error) = &prompt.last_error {
        ui::warn(&format!("Ответ Apple: {error}"));
    }
    match prompt.method {
        Method::Devices => ui::status("Apple отправила код проверки на другие ваши устройства."),
        Method::Sms => ui::status(&format!(
            "Apple отправила код проверки по SMS на {}.",
            prompt.number(prompt.selected)
        )),
        Method::Voice => ui::status(&format!(
            "Apple звонит на {} и продиктует код проверки.",
            prompt.number(prompt.selected)
        )),
        Method::Choose => ui::status("Выберите, как Apple отправит код проверки."),
    }
    for (id, number, mode) in &prompt.numbers {
        let how = if mode == "voice" {
            " (только звонок)"
        } else {
            ""
        };
        ui::status(&format!("  {id}: {number}{how}"));
    }
    ui::status("Введите код; t N — SMS на номер N, c N — звонок, d — отправить на устройства,");
    ui::status("r — отправить ещё раз, q — отменить вход.");
    loop {
        let Some(line) = ui::ask("Код: ") else {
            return TwoFactorCallbackResponse::Abort;
        };
        match parse_two_factor(&line) {
            Some(answer) => return answer,
            None if line.is_empty() => continue,
            None => ui::warn("Введите шестизначный код или одну из команд: t N, c N, d, r, q."),
        }
    }
}

fn parse_two_factor(line: &str) -> Option<TwoFactorCallbackResponse> {
    let line = line.trim();
    let digits: String = line
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit()) {
        return Some(TwoFactorCallbackResponse::SubmitCode(digits));
    }
    let mut words = line.split_whitespace();
    let (Some(verb), id) = (
        words.next(),
        words.next().and_then(|n| n.parse::<u32>().ok()),
    ) else {
        return None;
    };
    match (verb.to_ascii_lowercase().as_str(), id) {
        ("t" | "text" | "sms", Some(id)) => Some(TwoFactorCallbackResponse::SendSms(id)),
        ("c" | "call" | "voice", Some(id)) => Some(TwoFactorCallbackResponse::CallNumber(id)),
        ("d" | "devices", None) => Some(TwoFactorCallbackResponse::SendToDevices),
        ("r" | "resend", None) => Some(TwoFactorCallbackResponse::ResendCode),
        ("q" | "quit", None) => Some(TwoFactorCallbackResponse::Abort),
        _ => None,
    }
}

#[allow(clippy::ptr_arg)]
fn choose_certificates_to_revoke(certs: &Vec<DevelopmentCertificate>) -> Option<Vec<String>> {
    tokio::task::block_in_place(|| {
        if !ui::interactive() {
            return choose_certificates_plain(certs);
        }
        ui::warn(
            "У этой учётной записи достигнут лимит сертификатов подписи Apple. Чтобы создать сертификат siboot, нужно отозвать один из существующих. Подписанные им приложения перестанут открываться на всех устройствах, пока их не переподпишут. Отменить отзыв нельзя.",
        );
        let mut labels: Vec<String> = certs
            .iter()
            .map(|cert| {
                format!(
                    "{} · \"{}\" · {}",
                    cert.name.as_deref().unwrap_or("сертификат"),
                    cert.machine_name.as_deref().unwrap_or("?"),
                    cert.serial_number.as_deref().unwrap_or("?"),
                )
            })
            .collect();
        labels.push("Не отзывать — остановить".into());
        let index = ui::choose("какой сертификат отозвать?", &labels)?;
        let serial = certs.get(index)?.serial_number.clone()?;
        if !ui::confirm("отозвать сертификат? это нельзя отменить", false)
        {
            return None;
        }
        ui::status(&format!("отзыв сертификата {serial}"));
        Some(vec![serial])
    })
}

fn choose_certificates_plain(certs: &[DevelopmentCertificate]) -> Option<Vec<String>> {
    println!();
    ui::warn("Достигнут лимит сертификатов подписи Apple для этой учётной записи.");
    ui::warn("Чтобы создать сертификат siboot, нужно отозвать существующий.");
    ui::warn("Приложения с ним перестанут открываться на всех устройствах. Отменить нельзя.");
    for (i, cert) in certs.iter().enumerate() {
        ui::status(&format!(
            "  {}: {} — создан на «{}», серийный номер {}",
            i + 1,
            cert.name.as_deref().unwrap_or("сертификат"),
            cert.machine_name.as_deref().unwrap_or("?"),
            cert.serial_number.as_deref().unwrap_or("?"),
        ));
    }
    let line = ui::ask("Номер сертификата для отзыва (пусто — отмена): ")?;
    let index = line.trim().parse::<usize>().ok()?.checked_sub(1)?;
    let serial = certs.get(index)?.serial_number.clone()?;
    ui::status(&format!("отзыв сертификата {serial}"));
    Some(vec![serial])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(answer: Option<TwoFactorCallbackResponse>, expected: &str) -> bool {
        format!("{answer:?}") == expected
    }

    #[test]
    fn two_factor_answers_parse() {
        assert!(is(
            parse_two_factor("123456"),
            "Some(SubmitCode(\"123456\"))"
        ));
        assert!(is(
            parse_two_factor(" 123 456 "),
            "Some(SubmitCode(\"123456\"))"
        ));
        assert!(is(
            parse_two_factor("123-456"),
            "Some(SubmitCode(\"123456\"))"
        ));
        assert!(is(parse_two_factor("t 2"), "Some(SendSms(2))"));
        assert!(is(parse_two_factor("call 1"), "Some(CallNumber(1))"));
        assert!(is(parse_two_factor("d"), "Some(SendToDevices)"));
        assert!(is(parse_two_factor("r"), "Some(ResendCode)"));
        assert!(is(parse_two_factor("q"), "Some(Abort)"));
        for bad in ["12345", "1234567", "t", "t x", "d 1", "hello", ""] {
            assert!(parse_two_factor(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn servers_go_last_good_first_and_a_pick_alone() {
        let all = candidates(None, Some("https://ani.iloader.app/"));
        assert_eq!(all[0], "https://ani.iloader.app");
        assert_eq!(
            all.len(),
            SERVERS.len(),
            "no duplicate of the last good one"
        );
        assert_eq!(candidates(None, None)[0], SERVERS[0]);
        assert_eq!(
            candidates(None, None)[..2],
            ["https://anisette.lillq.me", "https://new.ani.sidestore.io"]
        );
        let after_mac = candidates(None, Some("https://new.ani.sidestore.io"));
        let lillq = after_mac.iter().position(|s| s == POOL).unwrap();
        assert_eq!(lillq, SERVERS.len() - 2, "{after_mac:?}");
        assert_eq!(after_mac.last().unwrap(), WINDOWS_SERVER);
        assert_eq!(after_mac.len(), SERVERS.len());
        assert_eq!(candidates(None, Some(POOL)), SERVERS);
        assert_eq!(candidates(None, Some(WINDOWS_SERVER))[1], POOL);
        assert_eq!(
            candidates(
                Some("https://example.org/"),
                Some("https://ani.iloader.app")
            ),
            ["https://example.org"]
        );
    }

    #[test]
    fn credential_errors_stop_and_server_errors_move_on() {
        assert!(stops_sign_in("login failed: -22406 Enter the correct password").is_some());
        assert!(stops_sign_in("login failed: HTTP 429").is_some());
        assert!(stops_sign_in("login failed: No 2FA code provided, aborting").is_some());
        assert!(stops_sign_in("anisette provider: error sending request").is_none());
        assert!(stops_sign_in(apple_session::ANDROID_ADI_SKIP).is_none());
    }

    #[test]
    fn a_folder_made_under_a_capitalized_email_is_kept() {
        let storage = std::env::temp_dir().join(format!("siboot-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&storage);
        let typed = "Someone@Example.com";
        let apple_id = typed.to_lowercase();
        let old = apple_session::account_dir(&storage, typed);
        let new = apple_session::account_dir(&storage, &apple_id);

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("key"), "old key").unwrap();
        adopt_folder_typed_as(&storage, typed, &apple_id);
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(new.join("key")).unwrap(), "old key");

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("key"), "other key").unwrap();
        adopt_folder_typed_as(&storage, typed, &apple_id);
        assert_eq!(std::fs::read_to_string(new.join("key")).unwrap(), "old key");
        assert!(old.exists());

        std::fs::remove_dir_all(&storage).unwrap();
    }

    const NOW: u64 = 1_789_300_000;

    fn store(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("siboot-token-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn save_token(storage: &Path, apple_id: &str, expiry: u64, duration: u64, saved_at: u64) {
        let dir = apple_session::account_dir(storage, apple_id);
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"apple_id":"{apple_id}","adsid":"001234-05-abcdef","token":"AAAABLwIAAAAAGW","duration":{duration},"expiry":{expiry},"saved_at":{saved_at}}}"#
        );
        std::fs::write(dir.join(SESSION_FILE), json).unwrap();
    }

    #[test]
    fn a_saved_token_is_tried_until_it_runs_out() {
        let storage = store("expiry");
        let id = "someone@example.com";
        assert!(matches!(saved_token(&storage, id, NOW), Saved::None));

        save_token(&storage, id, (NOW + 86_400) * 1000, 0, NOW - 100);
        let Saved::Usable(token) = saved_token(&storage, id, NOW) else {
            panic!("a day left is usable");
        };
        assert_eq!(token.adsid, "001234-05-abcdef");
        assert_eq!(token.app_token().token, "AAAABLwIAAAAAGW");
        assert_eq!(token.lifetime(NOW), "Apple's sign-in lasts 1d 0h more");

        save_token(&storage, id, NOW + 300, 0, NOW - 100);
        assert!(matches!(saved_token(&storage, id, NOW), Saved::RunOut));
        save_token(&storage, id, 0, 3_600, NOW - 7_200);
        assert!(matches!(saved_token(&storage, id, NOW), Saved::RunOut));

        save_token(&storage, id, 0, 0, NOW - 100);
        assert!(matches!(saved_token(&storage, id, NOW), Saved::Usable(_)));

        save_token(&storage, id, 0, 0, NOW);
        let dir = apple_session::account_dir(&storage, id);
        let other = std::fs::read_to_string(dir.join(SESSION_FILE))
            .unwrap()
            .replace(id, "someone.else@example.com");
        std::fs::write(dir.join(SESSION_FILE), other).unwrap();
        assert!(matches!(saved_token(&storage, id, NOW), Saved::None));
        std::fs::write(dir.join(SESSION_FILE), "{not json").unwrap();
        assert!(matches!(saved_token(&storage, id, NOW), Saved::None));

        std::fs::remove_dir_all(&storage).unwrap();
    }

    #[test]
    fn the_token_is_read_where_apple_session_keeps_it() {
        let storage = store("path");
        let id = "someone@example.com";
        save_token(&storage, id, 0, 0, NOW);
        assert!(matches!(saved_token(&storage, id, NOW), Saved::Usable(_)));
        assert!(apple_session::forget(&storage, id));
        assert!(matches!(saved_token(&storage, id, NOW), Saved::None));
        std::fs::remove_dir_all(&storage).unwrap();
    }

    #[test]
    fn accounts_that_signed_in_before_start_the_list_latest_first() {
        let storage = store("before");
        let home = storage.join("home");
        save_token(&storage, "old@example.com", 0, 0, NOW - 1_000);
        save_token(&storage, "new@example.com", 0, 0, NOW);
        save_token(&storage, "moved@example.com", 0, 0, NOW + 1);
        std::fs::rename(
            apple_session::account_dir(&storage, "moved@example.com"),
            storage.join("elsewhere"),
        )
        .unwrap();
        std::fs::create_dir_all(storage.join("anisette")).unwrap();

        assert_eq!(
            signed_in_before(&storage),
            ["new@example.com", "old@example.com"]
        );
        start_list(&home, &storage);
        assert_eq!(saved::list(&home), ["new@example.com", "old@example.com"]);

        saved::forget(&home, "old@example.com").unwrap();
        start_list(&home, &storage);
        assert_eq!(saved::list(&home), ["new@example.com"]);

        std::fs::remove_dir_all(&storage).unwrap();
    }

    #[test]
    fn a_new_account_is_said_to_be_kept_once() {
        let home = store("remember");
        assert!(remember_account(&home, "someone@example.com"));
        assert!(!remember_account(&home, "someone@example.com"));
        assert_eq!(saved::list(&home), ["someone@example.com"]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn only_apples_refusal_of_the_token_asks_for_the_password() {
        for refused in [
            " ● Developer error 1100: Your session has expired. Please log in.",
            " ● Developer request failed\n ● HTTP status client error (401 Unauthorized) for url (https://developerservices2.apple.com/)",
            "developer session:  ● Failed to extract developer request result",
        ] {
            assert!(refuses_token(refused), "{refused}");
        }
        for not_refused in [
            " ● Failed to get anisette headers\n ● HTTP status server error (522 <unknown status code>)",
            " ● error sending request for url (https://developerservices2.apple.com/services/QH65B2/listTeams.action)",
            " ● HTTP status client error (429 Too Many Requests) for url (https://developerservices2.apple.com/)",
            " ● Developer request failed\n ● HTTP status server error (503 Service Unavailable)",
            " ● Failed to list developer teams\n ● Developer error 2160: Please verify the email address associated with your Apple Account.",
            " ● Developer error 1102: (Apple's age refusal)",
        ] {
            assert!(!refuses_token(not_refused), "{not_refused}");
        }
    }

    #[test]
    fn the_revoke_prompt_may_block_where_the_pipeline_runs() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let answer = runtime.block_on(async { tokio::task::block_in_place(|| 7) });
        assert_eq!(answer, 7);
    }
}
