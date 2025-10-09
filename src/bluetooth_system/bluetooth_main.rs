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
use zbus::{Proxy, zvariant::OwnedObjectPath};

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

    #[cfg(target_os = "linux")]
    {
        info!("Running on Linux, attempting to get MAC address via zbus...");
        let adapter_name = central.adapter_info().await?;
        info!("Adapter name: {}", adapter_name);
        let object_path_str = format!("/org/bluez/{}", adapter_name);
        info!("Object path: {}", object_path_str);
        let object_path = OwnedObjectPath::try_from(object_path_str)?;
        let connection = zbus::Connection::system().await?;
        let proxy = Proxy::new(
            &connection,
            "org.bluez",
            object_path,
            "org.bluez.Adapter1",
        )
        .await?;
        info!("Connected to bluez adapter via D-Bus");
        let address_value: zbus::zvariant::Value = proxy.get_property("Address").await?;
        info!("Raw address value from D-Bus: {:?}", address_value);

        // zvariant::Valueから文字列を抽出
        my_mac_address_str = if let zbus::zvariant::Value::Str(s) = address_value {
            s.to_string()
        } else {
            return Err(anyhow!("Address property is not a string: {:?}", address_value));
        };

        info!("Parsed MAC address: {}", my_mac_address_str);
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

    let mut events = central.events().await?;
    info!("Scanning for BLE devices...");
    if let Err(e) = central.start_scan(ScanFilter::default()).await {
        error!("Failed to start scan: {:?}", e);
        return Err(e.into());
    }
    time::sleep(Duration::from_secs(2)).await;

    info!("Started listening for BLE events.");
    while let Some(event) = events.next().await {
        if let btleplug::api::CentralEvent::DeviceDiscovered(id)
        | btleplug::api::CentralEvent::DeviceUpdated(id) = event
        {
            on_event_receive(&central, &id, tx.clone(), Arc::clone(&sound_map)).await;
        }
    }
    Ok(())
}

/// Bluetoothイベント受信時の処理
#[instrument(skip(central, sender, sound_map))]
async fn on_event_receive(
    central: &Adapter,
    id: &PeripheralId,
    sender: mpsc::Sender<Arc<DeviceInfo>>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
) {
    if let Ok(p) = central.peripheral(&id).await {
        if let Ok(Some(props)) = p.properties().await {
            if let Some(rssi) = props.rssi {
                let address = p.address().to_string();
                let should_log;
                {
                    let sound_map = sound_map.lock().unwrap();
                    should_log = sound_map.contains_key(&address);
                }

                if should_log {
                    let device_info = Arc::new(DeviceInfo {
                        address,
                        rssi,
                        last_seen: Instant::now(),
                    });
                    debug!(device = ?device_info, "Device found");
                    if let Err(e) = sender.send(device_info).await {
                        error!("Failed to send device info through channel: {}", e);
                    }
                }
            }
        }
    }
}
