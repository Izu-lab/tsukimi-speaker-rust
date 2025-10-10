mod audio_system;
mod bluetooth_system;
mod connect_system;
pub mod proto;

use crate::audio_system::audio_main::audio_main;
use crate::bluetooth_system::bluetooth_main::bluetooth_scanner;
use crate::connect_system::connect_main::{connect_main, SystemEnabledState};
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

    // OSの判定をログに出力（コンパイル時）
    #[cfg(target_os = "linux")]
    info!("Application compiled for Linux");

    #[cfg(not(target_os = "linux"))]
    info!("Application compiled for non-Linux");

    info!("Spawning performance monitor task");
    tokio::spawn(
        async {
            use sysinfo::{Pid, System};
            let mut sys = System::new_all();
            let pid = Pid::from(std::process::id() as usize);
            loop {
                // 必要な情報のみを個別に更新（効率化）
                sys.refresh_cpu();
                sys.refresh_memory();
                sys.refresh_process(pid);

                let total_cpu_usage = sys.global_cpu_info().cpu_usage();

                let (process_cpu, process_mem) = if let Some(process) = sys.process(pid) {
                    (process.cpu_usage(), process.memory())
                } else {
                    (0.0, 0)
                };

                let total_mem = sys.total_memory();
                let used_mem = sys.used_memory();

                tracing::info!(
                "Perf: [CPU] Total: {:.1}%, Process: {:.1}% | [MEM] System: {:.2}/{:.2} GB, Process: {:.2} MB",
                total_cpu_usage,
                process_cpu,
                used_mem as f64 / 1_073_741_824.0,
                total_mem as f64 / 1_073_741_824.0,
                process_mem as f64 / 1_048_576.0
            );

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
        "tsukimi-main.mp3".to_string(),  // sound.mp3が存在しない場合のためデフォルトファイルに変更
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
            let sound_map_clone = Arc::clone(&sound_map);
            tokio::spawn(
                async move {
                    if let Err(e) = bluetooth_scanner(bt_tx, my_address_clone, sound_map_clone).await {
                        error!("Bluetooth scanner error: {:?}", e);
                    }
                }
                .instrument(tracing::info_span!("bluetooth_scanner_task")), 
            )
        };

    // 時間同期のためのmpscチャンネル
    let (time_sync_tx, time_sync_rx) = mpsc::channel::<u64>(32);

    // サウンド設定のためのmpscチャンネル
    let (sound_setting_tx, sound_setting_rx) = mpsc::channel::<SoundSetting>(32);

    // SE再生のためのmpscチャンネル
    let (se_tx, se_rx) = mpsc::channel::<audio_system::audio_main::SePlayRequest>(32);

    // システム有効化状態のためのbroadcastチャンネル（複数の受信者に配信）
    let (system_enabled_tx, _system_enabled_rx) = broadcast::channel::<SystemEnabledState>(32);

    // システム監視タスク用のAbortHandle
    let (shutdown_tx, _shutdown_rx) = mpsc::channel::<()>(1);

    // mpscからbroadcastへデータを転送するタスク
    info!("Spawning data forwarding task");
    let bcast_tx_clone = bcast_tx.clone();
    let mut forward_system_enabled_rx = system_enabled_tx.subscribe();
    let shutdown_tx_for_forward = shutdown_tx.clone();
    let forward_handle = tokio::spawn(
        async move {
            let mut system_enabled = true;

            loop {
                tokio::select! {
                    device_info_opt = bt_rx.recv() => {
                        if let Some(device_info) = device_info_opt {
                            // システム有効化状態を確認
                            while let Ok(state) = forward_system_enabled_rx.try_recv() {
                                system_enabled = state.enabled;
                                info!(enabled = system_enabled, "Forwarding task: System enabled state changed");

                                if !system_enabled {
                                    info!("System disabled - initiating shutdown");
                                    let _ = shutdown_tx_for_forward.send(()).await;
                                }
                            }

                            // システムが有効な場合のみデータを転送
                            if system_enabled {
                                debug!(?device_info, "Forwarding device info");
                                if bcast_tx_clone.send(device_info).is_err() {
                                    warn!("Failed to send device info to broadcast channel. No receivers?");
                                }
                            } else {
                                debug!(?device_info, "System disabled - skipping device info forwarding");
                            }
                        } else {
                            break;
                        }
                    }
                    _ = forward_system_enabled_rx.recv() => {
                        if let Ok(state) = forward_system_enabled_rx.try_recv() {
                            system_enabled = state.enabled;
                            info!(enabled = system_enabled, "Forwarding task: System enabled state changed");

                            if !system_enabled {
                                info!("System disabled - initiating shutdown");
                                let _ = shutdown_tx_for_forward.send(()).await;
                            }
                        }
                    }
                }
            }
        }
        .instrument(tracing::info_span!("forwarding_task")),
    );

    // gRPC通信を行うタスク
    info!("Spawning gRPC server task");
    let grpc_rx = bcast_tx.subscribe();
    let connect_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        let sound_setting_tx_clone = sound_setting_tx.clone();
        let se_tx_clone = se_tx.clone();
        let system_enabled_tx_clone = system_enabled_tx.clone();
        tokio::spawn(
            async move {
                if let Err(e) =
                    connect_main(grpc_rx, time_sync_tx, sound_setting_tx_clone, se_tx_clone, system_enabled_tx_clone, sound_map_clone, my_address_clone, current_points_clone).await
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
    let audio_system_enabled_rx = system_enabled_tx.subscribe();
    let audio_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        tokio::task::spawn_blocking(move || {
            let _span = tracing::info_span!("audio_playback_task").entered();
            audio_main(audio_rx, time_sync_rx, sound_setting_rx, se_rx, audio_system_enabled_rx, sound_map_clone, my_address_clone, current_points_clone)
        })
    };

    // オーディオ再生タスクの結果を待つ
    match audio_handle.await {
        Ok(Ok(_)) => info!("Audio playback finished successfully."),
        Ok(Err(e)) => error!("Audio playback error: {}", format!("{:?}", e)),
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
