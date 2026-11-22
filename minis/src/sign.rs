use std::io::Write as _;
use std::path::{Path, PathBuf};

use isideload::sideload::sideloader::MemoryLimit;

use crate::account::Session;
use crate::fail::{Fail, Result};
use crate::tunnel::DeviceInfo;
use crate::{ui, Args, VERSION};

pub const RELEASE_IPA: &str =
    "https://github.com/FrizzleM/SideInstaller/releases/latest/download/SideInstaller.ipa";

pub struct Signed {
    pub app_dir: PathBuf,
    pub bundle_id: String,
    pub name: String,
    pub version: Option<String>,
}

pub async fn fetch_ipa(args: &Args) -> Result<PathBuf> {
    match args.ipa.as_deref() {
        Some(source) if !source.starts_with("https://") && !source.starts_with("http://") => {
            let path = PathBuf::from(source);
            check_ipa(&path)?;
            ui::ok(&format!("используется {}", path.display()));
            Ok(path)
        }
        source => {
            let url = source.unwrap_or(RELEASE_IPA);
            let dir = args.home.join("downloads");
            std::fs::create_dir_all(&dir).map_err(|e| {
                Fail::new(
                    format!("Не удалось создать {}.", dir.display()),
                    e.to_string(),
                )
            })?;
            let (filename, label) = match source {
                Some(_) => {
                    let filename = url
                        .split(['?', '#'])
                        .next()
                        .unwrap_or(url)
                        .rsplit('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or("application.ipa")
                        .to_string();
                    (filename.clone(), filename)
                }
                None => ("SideInstaller.ipa".to_string(), "SideInstaller".to_string()),
            };
            download(url, &dir.join(filename), &label).await
        }
    }
}

async fn download(url: &str, dest: &Path, label: &str) -> Result<PathBuf> {
    ui::status(&format!("загрузка {label}"));
    ui::detail(url);
    let client = reqwest::Client::builder()
        .user_agent(format!("siboot/{VERSION}"))
        .build()
        .map_err(|e| Fail::new("Не удалось начать загрузку.", e.to_string()))?;
    let mut response = client.get(url).send().await.map_err(|e| {
        Fail::new(
            format!("Не удалось загрузить {label}. Проверьте интернет-соединение. LocalDevVPN передаёт только трафик к 10.7.0.1 и не влияет на интернет."),
            crate::error_text::error_text(&e),
        )
    })?;
    if !response.status().is_success() {
        return Err(Fail::new(
            format!(
                "Сервер ответил кодом {} при загрузке {label}. Попробуйте ещё раз через минуту.",
                response.status()
            ),
            url.to_string(),
        ));
    }
    let total = response.content_length();
    let partial = dest.with_extension("ipa.part");
    let mut file = std::fs::File::create(&partial).map_err(|e| {
        Fail::new(
            format!("Не удалось записать {}.", partial.display()),
            e.to_string(),
        )
    })?;
    let mut received: u64 = 0;
    loop {
        let chunk = response.chunk().await.map_err(|e| {
            Fail::new(
                format!("Загрузка {label} прервалась. Запустите siboot снова."),
                e.to_string(),
            )
        })?;
        let Some(chunk) = chunk else { break };
        file.write_all(&chunk).map_err(|e| {
            Fail::new(
                format!("Не удалось записать {}.", partial.display()),
                e.to_string(),
            )
        })?;
        received += chunk.len() as u64;
        if let Some(total) = total.filter(|t| *t > 0) {
            ui::progress("загружено", received, total);
        }
    }
    ui::progress_end();
    drop(file);
    std::fs::rename(&partial, dest).map_err(|e| {
        Fail::new(
            format!("Не удалось записать {}.", dest.display()),
            e.to_string(),
        )
    })?;
    check_ipa(dest)?;
    ui::ok(&format!(
        "{label} загружен ({:.1} МБ)",
        received as f64 / 1_048_576.0
    ));
    Ok(dest.to_path_buf())
}

fn check_ipa(path: &Path) -> Result<()> {
    let mut magic = [0u8; 4];
    let read =
        std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic));
    match read {
        Ok(()) if &magic == b"PK\x03\x04" => Ok(()),
        Ok(()) => Err(Fail::new(
            format!(
                "{} не является IPA. Возможно, GitHub вернул страницу ошибки; попробуйте ещё раз через минуту.",
                path.display()
            ),
            format!("first bytes {magic:02x?}"),
        )),
        Err(e) => Err(Fail::new(
            format!("Не удалось прочитать {}.", path.display()),
            e.to_string(),
        )),
    }
}

pub async fn sign(session: &mut Session, ipa: &Path, device: &DeviceInfo) -> Result<Signed> {
    ui::status("подписание (это может занять некоторое время)");
    let (app_dir, _special) = session
        .sideloader
        .sign_app(
            ipa.to_path_buf(),
            None,
            MemoryLimit::Off,
            true,
            Some((device.name.as_str(), device.udid.as_str())),
            None,
        )
        .await
        .map_err(|e| {
            let text = crate::error_text::report_text(&e);
            Fail::new(sign_advice(&text), text)
        })?;
    let (bundle_id, name, version) = bundle_info(&app_dir)?;
    ui::ok(&format!("{name} подписан; идентификатор: {bundle_id}"));
    Ok(Signed {
        app_dir,
        bundle_id,
        name,
        version,
    })
}

fn sign_advice(text: &str) -> String {
    if text.starts_with("device registration failed") {
        "Apple не зарегистрировала этот iPhone в команде разработчиков и не выдаст профиль подготовки."
            .into()
    } else if text.contains("7460") {
        "Apple не выдаёт сертификат подписи: по сообщению Apple, он уже существует (ошибка 7460)."
            .into()
    } else if text.contains("9401")
        || text
            .to_ascii_lowercase()
            .contains("maximum number of app ids")
    {
        "Для этой учётной записи достигнут недельный лимит идентификаторов приложений. Дождитесь истечения одного из них или используйте другую учётную запись."
            .into()
    } else {
        "Не удалось подписать приложение.".into()
    }
}

fn bundle_info(app_dir: &Path) -> Result<(String, String, Option<String>)> {
    let info: plist::Dictionary = plist::from_file(app_dir.join("Info.plist")).map_err(|e| {
        Fail::new(
            "Не удалось прочитать Info.plist подписанного приложения.",
            e.to_string(),
        )
    })?;
    let text = |key: &str| {
        info.get(key)
            .and_then(|v| v.as_string())
            .map(str::to_string)
    };
    let bundle_id = text("CFBundleIdentifier").ok_or_else(|| {
        Fail::new(
            "У подписанного приложения отсутствует идентификатор пакета.",
            "CFBundleIdentifier missing",
        )
    })?;
    let name = text("CFBundleDisplayName")
        .filter(|n| !n.is_empty())
        .or_else(|| text("CFBundleName"))
        .unwrap_or_else(|| "Приложение".into());
    let version = text("CFBundleShortVersionString").filter(|v| !v.is_empty());
    Ok((bundle_id, name, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_zip_passes_as_an_ipa() {
        let dir = std::env::temp_dir().join(format!("siboot-ipa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.ipa");
        std::fs::write(&good, b"PK\x03\x04rest").unwrap();
        assert!(check_ipa(&good).is_ok());
        let page = dir.join("page.ipa");
        std::fs::write(&page, b"<!DOCTYPE html>").unwrap();
        assert!(check_ipa(&page)
            .unwrap_err()
            .advice
            .contains("не является IPA"));
        assert!(check_ipa(&dir.join("missing.ipa")).is_err());
    }

    #[test]
    fn the_bundle_id_and_name_come_from_info_plist() {
        let dir = std::env::temp_dir().join(format!(
            "siboot-app-{}/SideInstaller.app",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut info = plist::Dictionary::new();
        info.insert(
            "CFBundleIdentifier".into(),
            "com.frizzlem.sideinstaller.ABCDE12345".into(),
        );
        info.insert("CFBundleName".into(), "SideInstaller".into());
        plist::to_file_xml(dir.join("Info.plist"), &info).unwrap();
        let (id, name, version) = bundle_info(&dir).unwrap();
        assert_eq!(id, "com.frizzlem.sideinstaller.ABCDE12345");
        assert_eq!(name, "SideInstaller");
        assert_eq!(version, None);
        info.insert("CFBundleShortVersionString".into(), "1.7.0".into());
        plist::to_file_xml(dir.join("Info.plist"), &info).unwrap();
        assert_eq!(bundle_info(&dir).unwrap().2.as_deref(), Some("1.7.0"));
    }
}
