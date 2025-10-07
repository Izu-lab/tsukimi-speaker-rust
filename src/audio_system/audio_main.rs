use crate::proto::proto::SoundSetting;
use crate::DeviceInfo;
use anyhow::Result;
use glib::object::ObjectExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn};

// 再生状態を管理するためのenum
enum PlaybackState {
    WaitingForFirstSync,
    Playing,
}

#[instrument(skip(rx, time_sync_rx, sound_map))]
pub fn audio_main(
    mut rx: broadcast::Receiver<Arc<DeviceInfo>>,
    mut time_sync_rx: mpsc::Receiver<u64>,
    mut sound_setting_rx: mpsc::Receiver<SoundSetting>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) -> Result<()> {
    info!("Audio system main loop started.");

    let sound_setting = Arc::new(Mutex::new(SoundSetting {
        id: "default".to_string(),
        max_volume_rssi: -40.0,
        min_volume_rssi: -90.0,
        max_volume: 1.0,
        min_volume: 0.0,
        is_muted: false,
    }));

    gst::init()?;
    info!("GStreamer initialized successfully.");

    let mut current_sound: Option<String> = None;
    let mut detected_devices = HashMap::<String, Arc<DeviceInfo>>::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    // 最初のパイプラインをplaybinで構築（tsukimi-main.mp3）
    let initial_path = std::fs::canonicalize("tsukimi-main.mp3").unwrap_or_else(|_| std::path::PathBuf::from("tsukimi-main.mp3"));
    let initial_uri = format!("file://{}", initial_path.to_string_lossy());
    let pipeline_str = format!(
        "playbin uri={} audio-filter=\"audioconvert ! capsfilter caps=\\\"audio/x-raw,format=F32LE,rate=44100,channels=2\\\" ! pitch name=pch ! audioconvert ! audioresample\" audio-sink=autoaudiosink",
        initial_uri
    );
    // playbin をPipeline にダウンキャスト
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .expect("playbin should be a Pipeline");

    // --- 同期関連の変数 ---
    let mut current_rate = 1.0f64;
    let mut pitch: Option<gst::Element> = None; // playbin配下のpitchを参照（必要時に解決）

    // 時間の基準点を保存す��変数
    let mut playback_start_time = Instant::now(); // 再生開始/seek時の実時間
    let mut initial_server_time_ns = 0u64;      // ↑の瞬間のサーバー時間
    let mut _last_file_switch_time = Instant::now(); // ファイル切り替えた時刻

    // 再生状態の初期値
    let mut playback_state = PlaybackState::WaitingForFirstSync;

    // ## 再生開始の同期 ##
    pipeline.set_state(gst::State::Paused)?;
    info!("Waiting for pipeline to pause...");
    let _ = pipeline.state(gst::ClockTime::from_seconds(10)); // 状態変更を待つ
    info!("Pipeline is Paused. Waiting for first time sync...");

    let bus = pipeline.bus().unwrap();

    // 時刻同期待機のタイムアウト
    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    'main_loop: loop {
        // --- GStreamerメッセージ処理 ---
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => {
                    info!("End-of-stream reached. Looping playback.");
                    if let Err(e) = pipeline.seek_simple(
                        gst::SeekFlags::FLUSH,
                        gst::ClockTime::from_seconds(0),
                    ) {
                        error!("Failed to seek to beginning for looping: {}", e);
                    }
                }
                MessageView::Error(err) => {
                    error!(
                        "GStreamer pipeline error: {}, Debug info: {:?}, Source: {:?}",
                        err.error(),
                        err.debug(),
                        err.src().map(|s| s.name())
                    );
                    break 'main_loop;
                }
                MessageView::Warning(warn) => {
                    warn!(
                        "GStreamer pipeline warning: {}, Debug info: {:?}",
                        warn.error(),
                        warn.debug()
                    );
                }
                MessageView::Info(info_msg) => {
                    info!(
                        "GStreamer pipeline info: {}, Debug info: {:?}",
                        info_msg.error(),
                        info_msg.debug()
                    );
                }
                MessageView::StateChanged(state_changed) => {
                    if state_changed.src().map(|s| s == &pipeline).unwrap_or(false) {
                        debug!(
                            "Pipeline state changed from {:?} to {:?}",
                            state_changed.old(),
                            state_changed.current()
                        );
                    }
                }
                _ => (),
            }
        }

        // --- 再生状態に応じた処理 ---
        match playback_state {
            PlaybackState::WaitingForFirstSync => {
                // 時刻同期のタイムアウトチェック
                if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    warn!("Time sync timeout. Starting playback without sync.");
                    pipeline.set_state(gst::State::Playing)?;
                    info!("Pipeline state set to Playing (no sync).");

                    // 時間の基準点を記録（現在時刻をベースに）
                    playback_start_time = Instant::now();
                    initial_server_time_ns = 0;

                    // 初期ファイルを設定
                    current_sound = Some("tsukimi-main.mp3".to_string());

                    playback_state = PlaybackState::Playing;
                    continue;
                }

                if let Ok(server_time_ns) = time_sync_rx.try_recv() {
                    info!(server_time_ns, "Received first time sync. Starting playback.");

                    let duration = match pipeline.query_duration::<gst::ClockTime>() {
                        Some(d) if d.nseconds() > 0 => d,
                        _ => {
                            warn!("Could not query duration yet. Retrying...");
                            continue;
                        }
                    };

                    let seek_time_ns = server_time_ns % duration.nseconds();
                    let seek_time = gst::ClockTime::from_nseconds(seek_time_ns);

                    pipeline.seek_simple(gst::SeekFlags::FLUSH, seek_time)?;
                    
                    info!("Waiting for seek to complete...");
                    if let Some(_) = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]) {
                        info!("Seek completed (AsyncDone received).");
                    } else {
                        error!("Seek confirmation (AsyncDone) not received within 5s. Aborting.");
                        break 'main_loop;
                    }

                    pipeline.set_state(gst::State::Playing)?;
                    info!(?seek_time, "Pipeline state set to Playing.");

                    // 時間の基準点を記録
                    playback_start_time = Instant::now();
                    initial_server_time_ns = server_time_ns;

                    // 初期ファイルを設定
                    current_sound = Some("tsukimi-main.mp3".to_string());

                    playback_state = PlaybackState::Playing;
                }
            }
            PlaybackState::Playing => {
                // --- 他の処理は変更なし ---
                if let Ok(new_setting) = sound_setting_rx.try_recv() {
                    info!(?new_setting, "Received new sound setting");
                    *sound_setting.lock().unwrap() = new_setting;
                }
                while let Ok(device_info) = rx.try_recv() {
                    detected_devices.insert(device_info.address.clone(), device_info);
                }
                if Instant::now().duration_since(last_cleanup) > CLEANUP_INTERVAL {
                    let initial_count = detected_devices.len();
                    detected_devices.retain(|_, device_info| Instant::now().duration_since(device_info.last_seen) < CLEANUP_INTERVAL);
                    if initial_count != detected_devices.len() {
                        debug!("Cleaned up old devices.");
                    }
                    last_cleanup = Instant::now();
                }

                if let Ok(server_time_ns) = time_sync_rx.try_recv() {
                    // --- 実時間ベースの同期ロジック ---
                    let server_elapsed = (server_time_ns - initial_server_time_ns) as i64;
                    let client_elapsed = playback_start_time.elapsed().as_nanos() as i64;
                    let diff_real_ns = server_elapsed - client_elapsed;
                    let diff_abs_s = (diff_real_ns.abs() as f64) / 1e9;

                    let mut new_rate = current_rate;

                    if diff_abs_s > 1.0 {
                        warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected, seeking.");

                        // durationを安全に取得
                        if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
                            if duration.nseconds() > 0 {
                                let seek_time_ns = server_time_ns % duration.nseconds();
                                let seek_time = gst::ClockTime::from_nseconds(seek_time_ns);
                                pipeline.seek_simple(gst::SeekFlags::FLUSH, seek_time)?;

                                if let Some(_) = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]) {
                                    info!("Seek completed.");
                                    // seek完了後、時間の基準点をリセット
                                    playback_start_time = Instant::now();
                                    initial_server_time_ns = server_time_ns;
                                    new_rate = 1.0;
                                } else {
                                    error!("Seek confirmation not received. Sync might be unstable.");
                                }
                            } else {
                                warn!("Duration is 0, skipping seek.");
                            }
                        } else {
                            warn!("Could not query duration, skipping seek.");
                        }
                    } else {
                        // 比例制御で再生レートを調整
                        let diff_s = diff_real_ns as f64 / 1e9;
                        const CORRECTION_TIME_S: f64 = 2.0;
                        let correction_rate = diff_s / CORRECTION_TIME_S;
                        new_rate = (1.0 + correction_rate).clamp(0.9, 1.1);
                    }

                    if (new_rate - current_rate).abs() > 1e-9 {
                        if pitch.is_none() {
                            pitch = pipeline.by_name("pch");
                        }
                        if let Some(ref p) = pitch {
                            p.set_property("tempo", new_rate as f32);
                        } else {
                            warn!("pitch element not found; skipping tempo update");
                        }
                        current_rate = new_rate;
                    }

                    info!(
                        diff_ms = diff_real_ns / 1_000_000,
                        current_rate,
                        new_rate,
                        "Time sync processed."
                    );
                }

                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    let my_addr_opt_clone = my_address.lock().unwrap().clone();
                    let points = *current_points.lock().unwrap();

                    info!(
                        sound_map_size = sound_map.len(),
                        detected_devices_count = detected_devices.len(),
                        ?sound_map,
                        "Checking for best device"
                    );

                    let mut candidates: Vec<_> = detected_devices
                        .values()
                        .filter(|d| {
                            let has_key = sound_map.contains_key(&d.address);
                            let sound_file = sound_map.get(&d.address);
                            debug!(
                                address = %d.address,
                                rssi = d.rssi,
                                has_key = has_key,
                                sound_file = ?sound_file,
                                "Checking device"
                            );
                            has_key
                        })
                        .collect();

                    info!(candidates_count = candidates.len(), "Filtered candidates");

                    // ポイントとRSSIでソート
                    // 1. ポイントが高い順 (自分自身のデバイスであれば現在のポイント、そうでなければ0)
                    // 2. RSSIが高い順
                    candidates.sort_by(|a, b| {
                        let a_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                        let b_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                        b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                    });

                    let best = candidates.first().cloned();
                    if let Some(ref device) = best {
                        info!(
                            best_device_address = %device.address,
                            best_device_rssi = device.rssi,
                            "Selected best device"
                        );
                    } else {
                        info!("No best device found");
                    }
                    best
                };

                if let Some(device) = best_device {
                    let sound_map = sound_map.lock().unwrap();
                    if let Some(new_sound) = sound_map.get(&device.address) {
                        if current_sound.as_deref() != Some(new_sound.as_str()) {
                            info!("Switching sound to {}", new_sound);

                            // ファイルの存在確認
                            if !new_sound.is_empty() {
                                let sound_path = std::path::Path::new(new_sound);
                                if !sound_path.exists() {
                                    error!("Sound file does not exist: {}", new_sound);
                                    continue;
                                }
                                info!("Sound file exists: {}", new_sound);
                            } else {
                                warn!("Sound file name is empty, skipping switch.");
                                continue;
                            }

                            // playbin の URI を安全に切替 (Ready -> set uri -> Paused -> Playing)
                            if let Err(e) = pipeline.set_state(gst::State::Ready) {
                                error!("Failed to set pipeline to Ready state: {}", e);
                                continue;
                            }

                            let new_path = std::fs::canonicalize(new_sound).unwrap_or_else(|_| std::path::PathBuf::from(new_sound));
                            let new_uri = format!("file://{}", new_path.to_string_lossy());
                            // set_property は Result を返さないためそのまま設定
                            pipeline.set_property("uri", &new_uri);

                            // Paused へ
                            if let Err(e) = pipeline.set_state(gst::State::Paused) {
                                error!("Failed to set new pipeline to Paused: {}", e);
                                continue;
                            }
                            let _ = pipeline.state(gst::ClockTime::from_seconds(5));

                            // Playing へ
                            if let Err(e) = pipeline.set_state(gst::State::Playing) {
                                error!("Failed to set new pipeline to Playing: {}", e);
                                continue;
                            }

                            current_sound = Some(new_sound.clone());
                            playback_start_time = Instant::now();
                            initial_server_time_ns = 0;
                            _last_file_switch_time = Instant::now();
                            info!("Successfully switched to sound: {}", new_sound);
                        }
                    }
                } else {
                    // デフォルト音源への切替
                    let default_sound = "tsukimi-main.mp3";
                    if current_sound.as_deref() != Some(default_sound) {
                        info!("No device detected. Playing default sound: {}", default_sound);

                        if let Err(e) = pipeline.set_state(gst::State::Ready) {
                            error!("Failed to set pipeline to Ready state: {}", e);
                            continue;
                        }

                        let def_path = std::fs::canonicalize(default_sound).unwrap_or_else(|_| std::path::PathBuf::from(default_sound));
                        let def_uri = format!("file://{}", def_path.to_string_lossy());
                        // set_property は Result を返さないためそのまま設定
                        pipeline.set_property("uri", &def_uri);

                        if let Err(e) = pipeline.set_state(gst::State::Paused) {
                            error!("Failed to set pipeline to Paused: {}", e);
                            continue;
                        }
                        let _ = pipeline.state(gst::ClockTime::from_seconds(5));

                        if let Err(e) = pipeline.set_state(gst::State::Playing) {
                            error!("Failed to set pipeline to Playing: {}", e);
                            continue;
                        }

                        current_sound = Some(default_sound.to_string());
                        playback_start_time = Instant::now();
                        initial_server_time_ns = 0;
                        _last_file_switch_time = Instant::now();
                        info!("Default sound is now playing.");
                    }
                }
            }
        }
    }

    Ok(())
}
