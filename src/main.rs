mod audio_system;
mod bluetooth_system;
mod connect_system;
pub mod proto;

use crate::audio_system::audio_main::audio_main;
use crate::bluetooth_system::bluetooth_main::bluetooth_scanner;
use crate::connect_system::connect_main::connect_main;
use crate::proto::proto::SoundSetting;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn, Instrument};

#[derive(Debug, Clone)]
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

    info!("Spawning performance monitor task");
    tokio::spawn(
        async {
            use sysinfo::{Pid, System};
            let mut sys = System::new_all();
            // 自身のプロセスIDを取得
            let pid = Pid::from(std::process::id() as usize);
            loop {
                // CPU、メモリ、プロセスの情報を更新
                sys.refresh_cpu();
                sys.refresh_memory();
                sys.refresh_process(pid);

                // CPU全体の平均使用率
                let total_cpu_usage = sys.global_cpu_info().cpu_usage();

                // このプロセスのCPU使用率とメモリ使用量
                let (process_cpu, process_mem) = if let Some(process) = sys.process(pid) {
                    (process.cpu_usage(), process.memory())
                } else {
                    (0.0, 0)
                };

                // システム全体のメモリ使用量
                let total_mem = sys.total_memory();
                let used_mem = sys.used_memory();

                tracing::info!(
                "Perf: [CPU] Total: {:.1}%, Process: {:.1}% | [MEM] System: {:.2}/{:.2} GB, Process: {:.2} MB",
                total_cpu_usage,
                process_cpu,
                used_mem as f64 / 1_073_741_824.0, // GiB
                total_mem as f64 / 1_073_741_824.0, // GiB
                process_mem as f64 / 1_048_576.0 // MiB
            );

                // 5秒待機
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
        .instrument(tracing::info_span!("performance_monitor_task")),
    );

    info!("Starting application");

    // --- sound_mapの作成 ---
    let mut sound_map = HashMap::new();
    // TODO: ご自身の環境に合わせて、Bluetoothアドレスとサウンドファイル名を変更してください。
    sound_map.insert(
        "00:11:22:33:44:55".to_string(),
        "sound.mp3".to_string(),
    );
    let sound_map = Arc::new(Mutex::new(sound_map));
    let current_points = Arc::new(Mutex::new(0_i32));
    let my_address = Arc::new(Mutex::new(None::<String>));

    // Bluetoothスキャナからのデータを受け取るためのmpscチャンネル
    let (bt_tx, mut bt_rx) = mpsc::channel::<Arc<DeviceInfo>>(32);

    // 各タスクにデータを配信するためのbroadcastチャンネル
    let (bcast_tx, _) = broadcast::channel::<Arc<DeviceInfo>>(32);

    // Bluetoothスキャナをバックグラウンドタスクとして実行
    info!("Spawning bluetooth scanner task");
        let bluetooth_handle = {
            let my_address_clone = Arc::clone(&my_address);
            tokio::spawn(
                async move {
                    if let Err(e) = bluetooth_scanner(bt_tx, my_address_clone).await {
                        error!("Bluetooth scanner error: {}", e);
                    }
                }
                .instrument(tracing::info_span!("bluetooth_scanner_task")), 
            )
        };
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
    let (time_sync_tx, time_sync_rx) = mpsc::channel::<u64>(32);

    // サウンド設定のためのmpscチャンネル
    let (sound_setting_tx, sound_setting_rx) = mpsc::channel::<SoundSetting>(32);

    // gRPC通信を行うタスク
    info!("Spawning gRPC server task");
    let grpc_rx = bcast_tx.subscribe();
    let connect_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        let sound_setting_tx_clone = sound_setting_tx.clone();
        tokio::spawn(
            async move {
                if let Err(e) =
                    connect_main(grpc_rx, time_sync_tx, sound_setting_tx_clone, sound_map_clone, my_address_clone, current_points_clone).await
                {
                    error!("Connect server error: {}", e);
                }
            }
            .instrument(tracing::info_span!("grpc_server_task")),
        )
    };

    // 同期的なaudio_main関数をspawn_blockingで実行
    info!("Spawning audio playback task");
    let audio_rx = bcast_tx.subscribe();
    let audio_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        tokio::task::spawn_blocking(move || {
            let _span = tracing::info_span!("audio_playback_task").entered();
            audio_main(audio_rx, time_sync_rx, sound_setting_rx, sound_map_clone, my_address_clone, current_points_clone)
        })
    };

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
