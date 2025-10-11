use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use futures::stream::StreamExt;
use std::collections::HashMap;
use tracing::{debug, error, info, instrument};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;

#[cfg(target_os = "linux")]
use tracing::warn;

#[cfg(target_os = "linux")]
use zbus::{Proxy, zvariant::OwnedObjectPath};

// デバイス情報のキャッシュ構造体
struct DeviceCache {
    last_sent: Instant,
    last_rssi: i16,
}

/// Bluetoothデバイスをスキャンする非同期関数
#[instrument(skip(tx, my_address))]
pub async fn bluetooth_scanner(
    tx: mpsc::Sender<Arc<DeviceInfo>>,
    my_address: Arc<Mutex<Option<String>>>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
) -> Result<()> {
    info!("Starting Bluetooth scanner...");
    let manager = Manager::new().await?;
    info!("Bluetooth manager created.");
    let adapters = manager.adapters().await?;
    let central = adapters
        .into_iter()
        .nth(0)
        .ok_or_else(|| anyhow!("Bluetooth adapter not found"))?;

    // 自身のBluetoothアドレスを取得
    let my_mac_address_str: String;

    // OSの判定をログに出力
    #[cfg(target_os = "linux")]
    info!("Compiled for Linux target");

    #[cfg(not(target_os = "linux"))]
    info!("Compiled for non-Linux target");

    #[cfg(target_os = "linux")]
    {
        info!("Running on Linux, attempting to get MAC address via zbus...");

        let adapter_name = match central.adapter_info().await {
            Ok(name) => {
                info!("Adapter name: {}", name);
                name
            }
            Err(e) => {
                error!("Failed to get adapter info: {:?}", e);
                return Err(e.into());
            }
        };

        // adapter_nameから最初の単語（例: "hci0"）だけを抽出
        // "hci0 (usb:v1D6Bp0246d0552)" -> "hci0"
        let adapter_id = adapter_name.split_whitespace()
            .next()
            .unwrap_or(&adapter_name);
        info!("Extracted adapter ID: {}", adapter_id);

        let object_path_str = format!("/org/bluez/{}", adapter_id);
        info!("Object path: {}", object_path_str);

        let object_path = match OwnedObjectPath::try_from(object_path_str.clone()) {
            Ok(path) => {
                info!("Successfully created object path");
                path
            }
            Err(e) => {
                error!("Failed to create object path from '{}': {:?}", object_path_str, e);
                return Err(anyhow!("Invalid object path: {}", object_path_str));
            }
        };

        info!("Connecting to system D-Bus...");
        let connection = match zbus::Connection::system().await {
            Ok(conn) => {
                info!("Successfully connected to system D-Bus");
                conn
            }
            Err(e) => {
                error!("Failed to connect to system D-Bus: {:?}", e);
                return Err(anyhow!("D-Bus connection failed: {:?}", e));
            }
        };

        info!("Creating proxy for bluez adapter...");
        let proxy = match Proxy::new(
            &connection,
            "org.bluez",
            object_path,
            "org.bluez.Adapter1",
        )
        .await {
            Ok(p) => {
                info!("Successfully created proxy");
                p
            }
            Err(e) => {
                error!("Failed to create proxy: {:?}", e);
                return Err(anyhow!("Proxy creation failed: {:?}", e));
            }
        };

        info!("Getting Address property from D-Bus...");
        let address_value: zbus::zvariant::Value = match proxy.get_property("Address").await {
            Ok(val) => {
                info!("Successfully got Address property: {:?}", val);
                val
            }
            Err(e) => {
                error!("Failed to get Address property: {:?}", e);
                return Err(anyhow!("Failed to get Address property: {:?}", e));
            }
        };

        // zvariant::Valueから文字列を抽出
        my_mac_address_str = if let zbus::zvariant::Value::Str(s) = address_value {
            let mac = s.to_string();
            info!("Parsed MAC address: {}", mac);
            mac
        } else {
            error!("Address property is not a string: {:?}", address_value);
            return Err(anyhow!("Address property is not a string: {:?}", address_value));
        };

        // Linux固有: スキャンパラメータの最適化を試みる
        info!("Attempting to optimize BLE scan parameters for Linux...");
        if let Err(e) = optimize_linux_scan_parameters(&proxy).await {
            warn!("Failed to optimize scan parameters (continuing anyway): {:?}", e);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Running on a non-Linux OS, using adapter_info() as ID.");
        my_mac_address_str = central.adapter_info().await?;
    }

    info!(my_id = %my_mac_address_str, "Using adapter ID");

    // 自身のBluetoothアドレスを保存
    {
        let mut my_addr = my_address.lock().unwrap();
        *my_addr = Some(my_mac_address_str.clone());
        info!(my_addr = ?*my_addr, "My address updated");
    }

    // デバイスキャッシュを作成（頻繁な送信を抑制しつつ、重要な更新は通知）
    let device_cache: Arc<Mutex<HashMap<String, DeviceCache>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut events = central.events().await?;
    info!("Scanning for BLE devices...");

    // スキャンフィルタの設定（空のフィルタで全デバイスをスキャン）
    let scan_filter = ScanFilter::default();

    if let Err(e) = central.start_scan(scan_filter).await {
        error!("Failed to start scan: {:?}", e);
        return Err(e.into());
    }

    time::sleep(Duration::from_secs(2)).await;

    info!("Started listening for BLE events.");

    // 定期的にキャッシュをクリーンアップするタスク
    let cache_clone = Arc::clone(&device_cache);
    tokio::spawn(async move {
        loop {
            time::sleep(Duration::from_secs(30)).await;
            let mut cache = cache_clone.lock().unwrap();
            let before = cache.len();
            cache.retain(|_, v| v.last_sent.elapsed() < Duration::from_secs(60));
            let after = cache.len();
            if before != after {
                debug!("Cache cleanup: {} -> {} entries", before, after);
            }
        }
    });

    while let Some(event) = events.next().await {
        if let btleplug::api::CentralEvent::DeviceDiscovered(id)
        | btleplug::api::CentralEvent::DeviceUpdated(id) = event
        {
            on_event_receive(&central, &id, tx.clone(), Arc::clone(&sound_map), Arc::clone(&device_cache)).await;
        }
    }
    Ok(())
}

/// Linux固有: BlueZ経由でスキャンパラメータを最適化
#[cfg(target_os = "linux")]
async fn optimize_linux_scan_parameters(proxy: &Proxy<'_>) -> Result<()> {
    use zbus::zvariant::{Dict, Value, Type};
    use std::collections::HashMap;

    // スキャンパラメータの設定
    // - DuplicateData: 重複データを報告（true推奨 - 同じデバイスの更新を受け取る）
    // - Transport: BLE専用スキャン
    // - RSSI: RSSIフィルタリングを無効化（-127で全て受信）
    // - Pathloss: パスロスフィルタリングを無効化

    // HashMapを使用してフィルタを構築
    let mut filter_map = HashMap::new();
    filter_map.insert("DuplicateData", Value::Bool(true));
    filter_map.insert("Transport", Value::Str("le".into()));
    // RSSIフィルタを最小値に設定（全てのビーコンを受信）
    filter_map.insert("RSSI", Value::I16(-127));

    info!("Setting discovery filter with optimized parameters for Wi-Fi coexistence...");

    match proxy.call_method("SetDiscoveryFilter", &(filter_map,)).await {
        Ok(_) => {
            info!("Successfully set optimized discovery filter");
            Ok(())
        }
        Err(e) => {
            warn!("Failed to set discovery filter: {:?}", e);
            Err(anyhow!("SetDiscoveryFilter failed: {:?}", e))
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn optimize_linux_scan_parameters(_proxy: &()) -> Result<()> {
    Ok(())
}

/// Bluetoothイベント受信時の処理
#[instrument(skip(central, sender, device_cache))]
async fn on_event_receive(
    central: &Adapter,
    id: &PeripheralId,
    sender: mpsc::Sender<Arc<DeviceInfo>>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    device_cache: Arc<Mutex<HashMap<String, DeviceCache>>>,
) {
    // 最初にアドレスを取得（軽量な操作）
    if let Ok(p) = central.peripheral(&id).await {
        let address = p.address().to_string();

        // 早期リターン: sound_mapに含まれないデバイスは即座にスキップ
        // プロパティ取得前にフィルタリングすることでパフォーマンス向上
        if !sound_map.lock().unwrap().contains_key(&address) {
            return;
        }

        // ターゲットデバイスのみプロパティを取得
        if let Ok(Some(props)) = p.properties().await {
            if let Some(rssi) = props.rssi {
                // キャッシュをチェックして、送信すべきかを判定
                let should_send = {
                    let mut cache = device_cache.lock().unwrap();

                    if let Some(cached) = cache.get_mut(&address) {
                        let elapsed = cached.last_sent.elapsed();
                        let rssi_diff = (rssi - cached.last_rssi).abs();

                        // 以下の条件のいずれかを満たす場合に送信:
                        // 1. 25ms以上経過している（50ms→25msに短縮でさらに高速化）
                        // 2. RSSIが1dBm以上変化している
                        let should_send = elapsed >= Duration::from_millis(25) || rssi_diff >= 1;

                        if should_send {
                            cached.last_sent = Instant::now();
                            cached.last_rssi = rssi;
                        }

                        should_send
                    } else {
                        // 新しいデバイス - 必ず送信
                        cache.insert(address.clone(), DeviceCache {
                            last_sent: Instant::now(),
                            last_rssi: rssi,
                        });
                        true
                    }
                };

                if should_send {
                    let device_info = Arc::new(DeviceInfo {
                        address: address.clone(),
                        rssi,
                        last_seen: Instant::now(),
                    });
                    debug!(device = ?device_info, "Device found - sending update");
                    if let Err(e) = sender.send(device_info).await {
                        error!("Failed to send device info through channel: {}", e);
                    }
                } else {
                    // 送信をスキップしたことをトレース（詳細ログ）
                    debug!(address = %address, rssi = %rssi, "Skipping send (too soon or RSSI unchanged)");
                }
            }
        }
    }
}
