use std::path::Path;

use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::AfcClient;
use idevice::house_arrest::HouseArrestClient;
use idevice::RsdService;

use crate::fail::{chain, Fail, Result};
use crate::tunnel::Connection;
use crate::ui;

pub const PAIRING_PATH: &str = "/Library/Application Support/rp_pairing_file.plist";
const MERGED_PATH: &str = "/Library/Application Support/combined_pairing_file.plist";
const IMPORTED_NAME_KEY: &str = "importedPairingFileName";
const HANDED_OVER_NAME: &str = "siboot.plist";

pub async fn hand_over(
    connection: &mut Connection,
    bundle_id: &str,
    pairing_path: &Path,
) -> Result<()> {
    ui::activity("открытие хранилища SideInstaller");
    let pairing = std::fs::read(pairing_path).map_err(|e| {
        Fail::new(
            format!("Не удалось прочитать {}.", pairing_path.display()),
            e.to_string(),
        )
    })?;

    let house_arrest =
        HouseArrestClient::connect_rsd(&mut connection.adapter, &mut connection.handshake)
            .await
            .map_err(|e| {
                Fail::new(
                    "Не удалось подключиться к house_arrest на iPhone.",
                    chain(&e),
                )
            })?;
    let mut afc = house_arrest.vend_container(bundle_id).await.map_err(|e| {
        Fail::new(
            format!("Не удалось открыть хранилище {bundle_id}. Импортируйте файл сопряжения в SideInstaller вручную. Он находится в Minis по пути ~/.siboot/rp_pairing_file.plist."),
            chain(&e),
        )
    })?;

    for dir in ["/Library", "/Library/Application Support"] {
        let _ = afc.mk_dir(dir).await;
    }
    ui::activity("запись файла сопряжения");
    let written = write_verified(&mut afc, PAIRING_PATH, &pairing).await?;
    ui::ok(&format!(
        "файл сопряжения записан в SideInstaller ({written} байт; выполнена проверка чтением)"
    ));

    let _ = afc.remove(MERGED_PATH).await;

    let prefs_path = format!("/Library/Preferences/{bundle_id}.plist");
    match mark_imported(&mut afc, &prefs_path).await {
        Ok(()) => ui::detail(&format!(
            "файл отмечен как импортированный «{HANDED_OVER_NAME}» ({prefs_path})"
        )),
        Err(e) => {
            ui::warn(
                "Не удалось отметить файл сопряжения как импортированный. SideInstaller сможет подключиться, но может снова предложить выполнить сопряжение.",
            );
            ui::detail(&e.raw);
        }
    }
    Ok(())
}

async fn mark_imported(afc: &mut AfcClient, prefs_path: &str) -> Result<()> {
    let mut prefs = match read_if_present(afc, prefs_path).await? {
        Some(bytes) if !bytes.is_empty() => plist::from_bytes::<plist::Dictionary>(&bytes)
            .map_err(|e| {
                Fail::new(
                    "Настройки SideInstaller не являются файлом plist.",
                    e.to_string(),
                )
            })?,
        _ => plist::Dictionary::new(),
    };
    prefs.insert(IMPORTED_NAME_KEY.into(), HANDED_OVER_NAME.into());
    let mut bytes = Vec::new();
    plist::to_writer_binary(&mut bytes, &prefs).map_err(|e| {
        Fail::new(
            "Не удалось закодировать настройки SideInstaller.",
            e.to_string(),
        )
    })?;
    let _ = afc.mk_dir("/Library/Preferences").await;
    let staging = format!("{prefs_path}.siboot");
    write_verified(afc, &staging, &bytes).await?;
    afc.rename(staging, prefs_path)
        .await
        .map_err(|e| Fail::new("Не удалось сохранить настройки SideInstaller.", chain(&e)))?;
    Ok(())
}

async fn read_if_present(afc: &mut AfcClient, path: &str) -> Result<Option<Vec<u8>>> {
    if afc.get_file_info(path).await.is_err() {
        return Ok(None);
    }
    let mut file = afc
        .open(path, AfcFopenMode::RdOnly)
        .await
        .map_err(|e| Fail::new(format!("Не удалось открыть {path}."), chain(&e)))?;
    let bytes = file
        .read_entire()
        .await
        .map_err(|e| Fail::new(format!("Не удалось прочитать {path}."), chain(&e)))?;
    let _ = file.close().await;
    Ok(Some(bytes))
}

async fn write_verified(afc: &mut AfcClient, path: &str, data: &[u8]) -> Result<usize> {
    let mut file = afc
        .open(path, AfcFopenMode::WrOnly)
        .await
        .map_err(|e| Fail::new(format!("Не удалось создать {path}."), chain(&e)))?;
    file.write_entire(data)
        .await
        .map_err(|e| Fail::new(format!("Не удалось записать {path}."), chain(&e)))?;
    file.close()
        .await
        .map_err(|e| Fail::new(format!("Не удалось записать {path}."), chain(&e)))?;
    match read_if_present(afc, path).await? {
        Some(back) if back == data => Ok(data.len()),
        Some(back) => Err(Fail::new(
            format!("Содержимое {path} отличается от записанных данных."),
            format!("wrote {} bytes, read {}", data.len(), back.len()),
        )),
        None => Err(Fail::new(format!("Файл {path} исчез после записи."), "")),
    }
}
