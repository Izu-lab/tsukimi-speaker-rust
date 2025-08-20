use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use futures::stream::StreamExt;
use std::time::Duration;
use tokio::time;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Bluetoothデバイス管理マネージャーを初期化
    let manager = Manager::new().await?;

    // 最初に見つかったアダプターを取得
    let adapters = manager.adapters().await?;
    let central = adapters
        .into_iter()
        .nth(0)
        .ok_or("Bluetooth adapter not found")?;

    // イベントのストリームを取得
    let mut events = central.events().await?;

    // スキャンを開始
    println!("Scanning for BLE devices...");
    central.start_scan(ScanFilter::default()).await?;

    // OSがスキャン結果を収集するのを少し待つ
    time::sleep(Duration::from_secs(2)).await;

    // イベントをループで待機
    while let Some(event) = events.next().await {
        // デバイスが発見または更新されたイベントを処理
        if let btleplug::api::CentralEvent::DeviceDiscovered(id) |
        btleplug::api::CentralEvent::DeviceUpdated(id) = event {
            on_event_receive(&central, &id).await;
        }
    }

    Ok(())
}

async fn on_event_receive(central: &Adapter, id: &PeripheralId) {
    // デバイス（ペリフェラル）の情報を取得
    if let Ok(p) = central.peripheral(&id).await {
        if let Ok(Some(props)) = p.properties().await {
            // RSSIが取得できた場合のみ表示
            if let Some(rssi) = props.rssi {
                println!(
                    "Device: {:?} ({:?}), RSSI: {} dBm",
                    props.local_name.unwrap_or_else(|| "Unknown".to_string()),
                    p.address(),
                    rssi
                );
            }
        }
    }
}