mod audio_system;
mod bluetooth_system;
mod connect_system;
pub mod proto;

use crate::audio_system::audio_main::audio_main;
use crate::bluetooth_system::bluetooth_main::bluetooth_scanner;
use crate::connect_system::connect_main::connect_main;
use anyhow::Result;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn, Instrument};

#[derive(Debug)]
pub struct DeviceInfo {
    pub address: String,
    pub rssi: i16,
    pub last_seen: std::time::Instant,
}

#[instrument]
#[tokio::main]
async fn main() -> Result<()> {
    // tracingを初期化
    tracing_subscriber::fmt::init();

    info!("Starting application");

    // Bluetoothスキャナからのデータを受け取るためのmpscチャンネル
    let (bt_tx, mut bt_rx) = mpsc::channel::<Arc<DeviceInfo>>(32);

    // 各タスクにデータを配信するためのbroadcastチャンネル
    let (bcast_tx, _) = broadcast::channel::<Arc<DeviceInfo>>(32);

    // Bluetoothスキャナをバックグラウンドタスクとして実行
    info!("Spawning bluetooth scanner task");
    let bluetooth_handle = tokio::spawn(
        async move {
            if let Err(e) = bluetooth_scanner(bt_tx).await {
                error!("Bluetooth scanner error: {}", e);
            }
        }
        .instrument(tracing::info_span!("bluetooth_scanner_task")),
    );

    // mpscからbroadcastへデータを転送するタスク
    info!("Spawning data forwarding task");
    let bcast_tx_clone = bcast_tx.clone();
    let forward_handle = tokio::spawn(
        async move {
            while let Some(device_info) = bt_rx.recv().await {
                debug!(?device_info, "Forwarding device info");
                if bcast_tx_clone.send(device_info).is_err() {
                    warn!("Failed to send device info to broadcast channel. No receivers?");
                }
            }
        }
        .instrument(tracing::info_span!("forwarding_task")),
    );

    // 時間同期のためのmpscチャンネル
    let (time_sync_tx, time_sync_rx) = mpsc::channel::<String>(32);

    // gRPC通信を行うタスク
    info!("Spawning gRPC server task");
    let grpc_rx = bcast_tx.subscribe();
    let connect_handle = tokio::spawn(
        async move {
            if let Err(e) = connect_main(grpc_rx, time_sync_tx).await {
                error!("Connect server error: {}", e);
            }
        }
        .instrument(tracing::info_span!("grpc_server_task")),
    );

    // 同期的なaudio_main関数をspawn_blockingで実行
    info!("Spawning audio playback task");
    let audio_rx = bcast_tx.subscribe();
    let audio_handle = tokio::task::spawn_blocking(move || {
        let _span = tracing::info_span!("audio_playback_task").entered();
        audio_main(audio_rx, time_sync_rx)
    });

    // オーディオ再生タスクの結果を待つ
    match audio_handle.await {
        Ok(Ok(_)) => info!("Audio playback finished successfully."),
        Ok(Err(e)) => error!("Audio playback error: {e}"),
        Err(e) => error!("Audio task panicked: {e}"),
    }

    // アプリケーション終了時に各タスクを停止
    info!("Aborting tasks");
    bluetooth_handle.abort();
    forward_handle.abort();
    connect_handle.abort();

    info!("Application finished");
    Ok(())
}
