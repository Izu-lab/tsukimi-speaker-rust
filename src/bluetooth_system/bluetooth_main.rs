use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use futures::stream::StreamExt;
use tracing::{debug, error, info, instrument};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time;

/// Bluetoothデバイスをスキャンする非同期関数
#[instrument(skip(tx, my_address))]
pub async fn bluetooth_scanner(
    tx: mpsc::Sender<Arc<DeviceInfo>>,
    my_address: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    info!("Starting Bluetooth scanner...");
    let manager = Manager::new().await?;
    info!("Bluetooth manager created.");
    let adapters = manager.adapters().await?;
    let central = adapters
        .into_iter()
        .nth(0)
        .ok_or_else(|| anyhow!("Bluetooth adapter not found"))?;

    let adapter_info = central.adapter_info().await?;
    info!(?adapter_info, "Using Bluetooth adapter");

    // 自身のBluetoothアドレスを保存
    {
        let mut my_addr = my_address.lock().unwrap();
        *my_addr = Some(adapter_info.clone());
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
            on_event_receive(&central, &id, tx.clone()).await;
        }
    }
    Ok(())
}

/// Bluetoothイベント受信時の処理
#[instrument(skip(central, sender))]
async fn on_event_receive(
    central: &Adapter,
    id: &PeripheralId,
    sender: mpsc::Sender<Arc<DeviceInfo>>,
) {
    if let Ok(p) = central.peripheral(&id).await {
        if let Ok(Some(props)) = p.properties().await {
            if let Some(rssi) = props.rssi {
                let device_info = Arc::new(DeviceInfo {
                    address: p.address().to_string(),
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
