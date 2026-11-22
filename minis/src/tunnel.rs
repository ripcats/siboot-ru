use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use idevice::lockdown::LockdownClient;
use idevice::remote_pairing::{
    connect_tls_psk_tunnel_native, RemotePairingClient, RpPairingFile, RpPairingSocket,
};
use idevice::rsd::RsdHandshake;
use idevice::tcp::adapter::Adapter;
use idevice::tcp::handle::AdapterHandle;
use idevice::RsdService;
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::fail::{chain, Fail, Result};
use crate::ui;

const HOSTNAME: &str = "siboot";

pub struct Connection {
    pub adapter: AdapterHandle,
    pub handshake: RsdHandshake,
}

pub struct DeviceInfo {
    pub name: String,
    pub product_type: String,
    pub ios_version: String,
    pub udid: String,
}

pub async fn verify(addr: SocketAddr, path: &Path) -> Result<()> {
    let mut file = read_pairing(path).await?;
    pair_verified(addr, &mut file).await.map(|_| ())
}

pub async fn open(addr: SocketAddr, path: &Path) -> Result<Connection> {
    let mut file = read_pairing(path).await?;
    let mut rpc = pair_verified(addr, &mut file).await?;

    let port = step(
        "запрос туннельного порта у iPhone",
        15,
        rpc.create_tcp_listener(),
    )
    .await?;
    ui::detail(&format!("туннельный порт: {port}"));
    ui::activity("подключение к туннельному порту");
    let stream = match timeout(
        Duration::from_secs(10),
        TcpStream::connect((addr.ip(), port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(Fail::new(
                format!(
                    "iPhone открыл туннельный порт, но подключиться к {}:{port} не удалось.",
                    addr.ip()
                ),
                e.to_string(),
            ))
        }
        Err(_) => {
            return Err(Fail::new(
                format!(
                    "iPhone открыл туннельный порт, но {}:{port} не отвечает.",
                    addr.ip()
                ),
                "истекло время ожидания подключения",
            ))
        }
    };
    let tunnel = step(
        "запуск защищённого туннеля",
        20,
        connect_tls_psk_tunnel_native(stream, rpc.encryption_key()),
    )
    .await?;
    let client_ip: IpAddr = tunnel.info.client_address.parse().map_err(|e| {
        Fail::new(
            "Туннель вернул адрес в неизвестном формате.",
            format!("{e}"),
        )
    })?;
    let server_ip: IpAddr = tunnel.info.server_address.parse().map_err(|e| {
        Fail::new(
            "Туннель вернул адрес в неизвестном формате.",
            format!("{e}"),
        )
    })?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;
    ui::detail(&format!(
        "туннель открыт: {client_ip} → {server_ip}, MTU {mtu}, RSD на порту {rsd_port}"
    ));

    let mut adapter = Adapter::new(Box::new(tunnel.into_inner()), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60));
    let mut adapter = adapter.to_async_handle();
    ui::activity("подключение к RSD через туннель");
    let rsd = match timeout(Duration::from_secs(15), adapter.connect(rsd_port)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(Fail::new(
                "Туннель открыт, но RSD не отвечает.",
                e.to_string(),
            ))
        }
        Err(_) => {
            return Err(Fail::new(
                "Туннель открыт, но RSD не отвечает.",
                "истекло время ожидания",
            ))
        }
    };
    let handshake = step("получение списка служб iPhone", 15, RsdHandshake::new(rsd)).await?;
    ui::detail(&format!(
        "RSD: служб — {}, версия протокола — {}",
        handshake.services.len(),
        handshake.protocol_version
    ));
    Ok(Connection { adapter, handshake })
}

impl Connection {
    pub async fn describe(&mut self) -> Result<DeviceInfo> {
        let mut lockdown = step(
            "запрос сведений об iPhone у lockdownd",
            15,
            LockdownClient::connect_rsd(&mut self.adapter, &mut self.handshake),
        )
        .await?;
        let values = step(
            "запрос сведений об iPhone у lockdownd",
            15,
            lockdown.get_value(None, None),
        )
        .await?;
        let field = |key: &str| {
            values
                .as_dictionary()
                .and_then(|d| d.get(key))
                .and_then(|v| v.as_string())
                .map(str::to_string)
        };
        let udid = field("UniqueDeviceID").or_else(|| {
            self.handshake
                .properties
                .get("UniqueDeviceID")
                .and_then(|v| v.as_string())
                .map(str::to_string)
        });
        let udid = udid.ok_or_else(|| {
            Fail::new(
                "iPhone не сообщил UDID, необходимый для подписи.",
                "no UniqueDeviceID from lockdownd or RSD",
            )
        })?;
        Ok(DeviceInfo {
            name: field("DeviceName").unwrap_or_else(|| "iPhone".into()),
            product_type: field("ProductType").unwrap_or_else(|| "iPhone".into()),
            ios_version: field("ProductVersion").unwrap_or_else(|| "?".into()),
            udid,
        })
    }
}

async fn read_pairing(path: &Path) -> Result<RpPairingFile> {
    RpPairingFile::read_from_file(path).await.map_err(|e| {
        Fail::new(
            format!(
                "Не удалось прочитать файл сопряжения {}. Выполните сопряжение через rootshell (`./siboot-pair.wasm`), затем снова запустите siboot.",
                path.display()
            ),
            chain(&e),
        )
    })
}

async fn pair_verified(
    addr: SocketAddr,
    file: &mut RpPairingFile,
) -> Result<RemotePairingClient<RpPairingSocket<TcpStream>>> {
    let stream = match timeout(Duration::from_secs(10), TcpStream::connect(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(Fail::new(
            format!(
                "Не удалось подключиться к iPhone по адресу {addr}. Подключите LocalDevVPN и проверьте Wi-Fi."
            ),
            e.to_string(),
        )),
        Err(_) => {
            return Err(Fail::new(
                format!("По адресу {addr} нет ответа. Подключите LocalDevVPN и проверьте Wi-Fi."),
                "истекло время ожидания подключения",
            ))
        }
    };
    let mut rpc = RemotePairingClient::new(RpPairingSocket::new(stream), HOSTNAME);
    step("установление связи с iPhone", 10, rpc.attempt_pair_verify()).await?;
    ui::activity("проверка файла сопряжения");
    match timeout(Duration::from_secs(15), rpc.validate_pairing(file)).await {
        Ok(Ok(())) => Ok(rpc),
        Ok(Err(e)) => Err(Fail::new(
            "iPhone не принимает файл сопряжения: возможно, он удалён в Настройках или iPhone сброшен. Выполните сопряжение через rootshell (`./siboot-pair.wasm`), затем снова запустите siboot.",
            chain(&e),
        )),
        Err(_) => Err(Fail::new(
            "iPhone перестал отвечать во время проверки сопряжения.",
            "истекло время ожидания проверки сопряжения",
        )),
    }
}

pub async fn step<T, E: std::error::Error>(
    what: &str,
    secs: u64,
    future: impl std::future::Future<Output = std::result::Result<T, E>>,
) -> Result<T> {
    ui::activity(what);
    match timeout(Duration::from_secs(secs), future).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Fail::new(
            format!("Ошибка во время операции: {what}."),
            chain(&e),
        )),
        Err(_) => Err(Fail::new(
            format!("Истекло время ожидания операции: {what}."),
            format!("нет ответа за {secs} с"),
        )),
    }
}
