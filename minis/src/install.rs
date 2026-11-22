use std::path::{Path, PathBuf};

use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::AfcClient;
use idevice::installation_proxy::InstallationProxyClient;
use idevice::RsdService;

use crate::fail::{chain, Fail, Result};
use crate::tunnel::Connection;
use crate::ui;

const STAGING: &str = "/PublicStaging";
const UPLOAD_CHUNK: usize = 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub relative: String,
    pub is_dir: bool,
    pub size: u64,
}

pub async fn install(connection: &mut Connection, app_dir: &Path) -> Result<String> {
    let bundle_name = app_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| {
            Fail::new(
                "У подписанного приложения нет имени каталога.",
                app_dir.display().to_string(),
            )
        })?;
    let remote_root = format!("{STAGING}/{bundle_name}");
    let entries = bundle_entries(app_dir).map_err(|e| {
        Fail::new(
            format!("Не удалось прочитать {}.", app_dir.display()),
            e.to_string(),
        )
    })?;
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let files = entries.iter().filter(|e| !e.is_dir).count();
    let size = format!("{:.1} МБ, файлов: {files}", total as f64 / 1_048_576.0);
    ui::status(&format!(
        "загрузка файлов: {files} ({:.1} МБ)",
        total as f64 / 1_048_576.0
    ));

    let mut afc = AfcClient::connect_rsd(&mut connection.adapter, &mut connection.handshake)
        .await
        .map_err(|e| Fail::new("Не удалось открыть AFC на iPhone.", chain(&e)))?;
    let _ = afc.remove_all(remote_root.clone()).await;
    let _ = afc.mk_dir(STAGING).await;
    afc.mk_dir(remote_root.clone()).await.map_err(|e| {
        Fail::new(
            format!("Не удалось создать {remote_root} на iPhone."),
            chain(&e),
        )
    })?;

    let started = std::time::Instant::now();
    let mut sent: u64 = 0;
    for entry in &entries {
        let remote = format!("{remote_root}/{}", entry.relative);
        if entry.is_dir {
            afc.mk_dir(remote.clone())
                .await
                .map_err(|e| Fail::new(format!("Не удалось создать {remote}."), chain(&e)))?;
            continue;
        }
        let bytes = std::fs::read(app_dir.join(&entry.relative)).map_err(|e| {
            Fail::new(
                format!("Не удалось прочитать {}.", entry.relative),
                e.to_string(),
            )
        })?;
        let mut file = afc
            .open(remote.clone(), AfcFopenMode::WrOnly)
            .await
            .map_err(|e| Fail::new(format!("Не удалось создать {remote} на iPhone."), chain(&e)))?;
        for piece in bytes.chunks(UPLOAD_CHUNK) {
            file.write_entire(piece).await.map_err(|e| {
                Fail::new(
                    format!("Передача файла {} прервалась.", entry.relative),
                    chain(&e),
                )
            })?;
            sent += piece.len() as u64;
            ui::progress("передано", sent, total);
        }
        file.close().await.map_err(|e| {
            Fail::new(
                format!("Передача файла {} прервалась.", entry.relative),
                chain(&e),
            )
        })?;
    }
    ui::progress("передано", total, total);
    ui::progress_end();
    ui::detail(&format!(
        "время загрузки: {:.1} с",
        started.elapsed().as_secs_f64()
    ));

    ui::status("проверка целостности загруженных файлов");
    let mut missing = Vec::new();
    for entry in entries.iter().filter(|e| !e.is_dir) {
        let remote = format!("{remote_root}/{}", entry.relative);
        match afc.get_file_info(remote).await {
            Ok(info) if info.size as u64 == entry.size => {}
            Ok(info) => missing.push(format!(
                "{} ({} of {} bytes)",
                entry.relative, info.size, entry.size
            )),
            Err(_) => missing.push(entry.relative.clone()),
        }
    }
    if !missing.is_empty() {
        return Err(Fail::new(
            format!(
                "Файлы не полностью переданы на iPhone (количество: {}). Запустите siboot снова.",
                missing.len()
            ),
            missing.join(", "),
        ));
    }
    ui::ok("проверка загрузки завершена: все файлы переданы полностью");

    let mut proxy =
        InstallationProxyClient::connect_rsd(&mut connection.adapter, &mut connection.handshake)
            .await
            .map_err(|e| {
                Fail::new(
                    "Не удалось подключиться к installation_proxy на iPhone.",
                    chain(&e),
                )
            })?;
    let mut options = plist::Dictionary::new();
    options.insert("PackageType".into(), "Developer".into());
    ui::status("установка");
    let installed = proxy
        .install_with_callback(
            remote_root.clone(),
            Some(plist::Value::Dictionary(options)),
            |(percent, _): (u64, ())| async move {
                ui::percent("установлено", percent);
            },
            (),
        )
        .await;
    ui::progress_end();
    if let Err(e) = installed {
        let raw = chain(&e);
        return Err(Fail::new(install_advice(&raw), raw));
    }
    let _ = afc.remove_all(remote_root).await;
    Ok(size)
}

fn install_advice(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("maximum number of installed apps")
        || lower.contains("maximum number of apps for free development profiles")
    {
        "На iPhone уже установлены 3 приложения, подписанные бесплатной учётной записью Apple. Удалите одно и снова запустите siboot.".into()
    } else if lower.contains("0xe8008015") || lower.contains("provisioning") {
        "installd отклонил профиль подготовки. Запустите siboot снова; если ошибка повторится, возможно, в профиль не включён этот iPhone."
            .into()
    } else {
        "installd отказал в установке приложения.".into()
    }
}

pub fn bundle_entries(root: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Entry>) -> std::io::Result<()> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    children.sort();
    for path in children {
        let relative = path
            .strip_prefix(root)
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        let meta = std::fs::metadata(&path)?;
        if meta.is_dir() {
            out.push(Entry {
                relative,
                is_dir: true,
                size: 0,
            });
            walk(root, &path, out)?;
        } else {
            out.push(Entry {
                relative,
                is_dir: false,
                size: meta.len(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_come_before_their_files_and_sizes_add_up() {
        let root =
            std::env::temp_dir().join(format!("siboot-bundle-{}/Demo.app", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Frameworks/Core.framework")).unwrap();
        std::fs::write(root.join("Info.plist"), b"1234").unwrap();
        std::fs::write(root.join("Demo"), vec![0u8; 100]).unwrap();
        std::fs::write(root.join("Frameworks/Core.framework/Core"), vec![0u8; 10]).unwrap();
        let entries = bundle_entries(&root).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.relative.as_str()).collect();
        assert_eq!(
            names,
            [
                "Demo",
                "Frameworks",
                "Frameworks/Core.framework",
                "Frameworks/Core.framework/Core",
                "Info.plist"
            ]
        );
        assert_eq!(entries.iter().map(|e| e.size).sum::<u64>(), 114);
        let framework = names
            .iter()
            .position(|n| *n == "Frameworks/Core.framework")
            .unwrap();
        let inside = names
            .iter()
            .position(|n| *n == "Frameworks/Core.framework/Core")
            .unwrap();
        assert!(framework < inside);
    }

    #[test]
    fn install_refusals_get_advice() {
        assert!(
            install_advice("ApplicationVerificationFailed: 0xe8008015 profile")
                .contains("профиль подготовки")
        );
        assert!(
            install_advice("This device has reached the maximum number of installed apps")
                .contains("3 приложения")
        );
        assert!(install_advice("something else").contains("отказал"));
    }
}
