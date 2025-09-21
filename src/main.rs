mod audio_system;
mod bluetooth_system;
mod connect_system;
pub mod proto;

use crate::audio_system::audio_main::audio_main;
use crate::bluetooth_system::bluetooth_main::bluetooth_scanner;
use crate::connect_system::connect_main::connect_main;
use anyhow::Result;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub address: String,
    pub rssi: i16,
    pub last_seen: std::time::Instant,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Bluetoothスキャナからのデータを受け取るためのmpscチャンネル
    let (bt_tx, mut bt_rx) = mpsc::channel::<DeviceInfo>(32);

    // 各タスクにデータを配信するためのbroadcastチャンネル
    let (bcast_tx, _) = broadcast::channel::<DeviceInfo>(32);

    // Bluetoothスキャナをバックグラウンドタスクとして実行
    let bluetooth_handle = tokio::spawn(async move {
        if let Err(e) = bluetooth_scanner(bt_tx).await {
            eprintln!("Bluetooth scanner error: {}", e);
        }
    });

    // mpscからbroadcastへデータを転送するタスク
    let bcast_tx_clone = bcast_tx.clone();
    let forward_handle = tokio::spawn(async move {
        while let Some(device_info) = bt_rx.recv().await {
            if bcast_tx_clone.send(device_info).is_err() {
                // 受信側がいない場合はエラーになるが、無視して続ける
            }
        }
    });

    // 時間同期のためのmpscチャンネル
    let (time_sync_tx, time_sync_rx) = mpsc::channel::<String>(32);

    // gRPC通信を行うタスク
    let grpc_rx = bcast_tx.subscribe();
    let connect_handle = tokio::spawn(async move {
        if let Err(e) = connect_main(grpc_rx, time_sync_tx).await {
            eprintln!("Connect server error: {}", e);
        }
    });

    // 同期的なaudio_main関数をspawn_blockingで実行
    let audio_rx = bcast_tx.subscribe();
    let audio_handle =
        tokio::task::spawn_blocking(move || audio_main(audio_rx, time_sync_rx));

    // オーディオ再生タスクの結果を待つ
    match audio_handle.await {
        Ok(Ok(_)) => println!("Audio playback finished successfully."),
        Ok(Err(e)) => eprintln!("Audio playback error: {e}"),
        Err(e) => eprintln!("Audio task panicked: {e}"),
    }

    // アプリケーション終了時に各タスクを停止
    bluetooth_handle.abort();
    forward_handle.abort();
    connect_handle.abort();

    Ok(())
}
