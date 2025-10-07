use crate::proto::proto::SoundSetting;
use crate::DeviceInfo;
use anyhow::Result;
use glib::object::Cast;
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



    let mut current_sound = Some("tsukimi-main.mp3".to_string());
    let mut detected_devices = HashMap::<String, Arc<DeviceInfo>>::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

    let pipeline_str = "filesrc name=src location=tsukimi-main.mp3 ! decodebin ! volume name=vol ! audioconvert ! capsfilter caps=\"audio/x-raw,format=F32LE,rate=44100,channels=2\" ! pitch name=pch ! audioconvert ! audioresample ! queue ! pulsesink";
    let pipeline = gst::parse::launch(pipeline_str)?;
    let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();

    let filesrc = pipeline.by_name("src").unwrap();
    let volume = pipeline.by_name("vol").unwrap();
    let pitch = pipeline.by_name("pch").unwrap();

    // --- 同期関連の変数 ---
    let mut current_rate = 1.0f64;
    let mut playback_state = PlaybackState::WaitingForFirstSync;
    // 時間の基準点を保存する変数
    let mut playback_start_time = Instant::now(); // 再生開始/seek時の実時間
    let mut initial_server_time_ns = 0u64;      // ↑の瞬間のサーバー時間

    // ## 再生開始の同期 ##
    pipeline.set_state(gst::State::Paused)?;
    info!("Waiting for pipeline to pause...");
    pipeline.state(gst::ClockTime::from_seconds(10)); // 状態変更を待つ
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
                        pitch.set_property("tempo", new_rate as f32);
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

                            // ファイルの存在確認（パスがある場合）
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

                            // パイプラインを停止
                            if let Err(e) = pipeline.set_state(gst::State::Null) {
                                error!("Failed to set pipeline to Null state: {}", e);
                                continue;
                            }

                            // Null状態への遷移を待つ
                            match pipeline.state(gst::ClockTime::from_seconds(5)) {
                                (Ok(gst::StateChangeSuccess::Success), _, _) => {
                                    info!("Pipeline successfully transitioned to Null state");
                                }
                                (Ok(state), _, _) => {
                                    warn!("Pipeline state change result: {:?}", state);
                                }
                                (Err(e), _, _) => {
                                    error!("Failed to wait for Null state: {}", e);
                                    continue;
                                }
                            }

                            // ファイルパスを変更
                            filesrc.set_property("location", new_sound.clone());
                            info!("Set new location: {}", new_sound);

                            // Ready状態を経由してからPlayingに遷移
                            if let Err(e) = pipeline.set_state(gst::State::Ready) {
                                error!("Failed to set pipeline to Ready state: {}", e);
                                continue;
                            }
                            info!("Set pipeline to Ready state, waiting...");

                            // Ready状態への遷移を待つ
                            match pipeline.state(gst::ClockTime::from_seconds(5)) {
                                (Ok(gst::StateChangeSuccess::Success), gst::State::Ready, _) => {
                                    info!("Pipeline successfully transitioned to Ready state");
                                }
                                (Ok(state), current, _) => {
                                    warn!("Pipeline Ready state result: {:?}, current: {:?}", state, current);
                                }
                                (Err(e), _, _) => {
                                    error!("Failed to wait for Ready state: {}", e);
                                    continue;
                                }
                            }

                            // Pausedを経由
                            if let Err(e) = pipeline.set_state(gst::State::Paused) {
                                error!("Failed to set pipeline to Paused state: {}", e);
                                continue;
                            }
                            info!("Set pipeline to Paused state, waiting...");

                            // Paused状態への遷移を待つ（AsyncDoneまで待機）
                            let paused_timeout = Instant::now();
                            let mut paused_ok = false;
                            loop {
                                if Instant::now().duration_since(paused_timeout) > std::time::Duration::from_secs(10) {
                                    error!("Timeout waiting for Paused state");
                                    break;
                                }

                                // busメッセージをチェック（AsyncDoneを待つ）
                                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
                                    use gst::MessageView;
                                    match msg.view() {
                                        MessageView::AsyncDone(_) => {
                                            info!("AsyncDone received for Paused state");
                                        }
                                        MessageView::Error(err) => {
                                            error!(
                                                "GStreamer error during Paused transition: {}, Debug: {:?}",
                                                err.error(),
                                                err.debug()
                                            );
                                            break;
                                        }
                                        MessageView::StateChanged(sc) => {
                                            if sc.src().map(|s| s == &pipeline).unwrap_or(false) {
                                                info!(
                                                    "Pipeline state changed during Paused wait: {:?} -> {:?}",
                                                    sc.old(),
                                                    sc.current()
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }

                                // 現在の状態を確認
                                let (ret, current, pending) = pipeline.state(gst::ClockTime::ZERO);
                                match (ret, current, pending) {
                                    (Ok(_), gst::State::Paused, gst::State::VoidPending) => {
                                        info!("Pipeline successfully reached Paused state");
                                        paused_ok = true;
                                        break;
                                    }
                                    (Ok(_), curr, pend) => {
                                        debug!("Waiting for Paused: current={:?}, pending={:?}", curr, pend);
                                    }
                                    (Err(e), curr, _) => {
                                        error!("Error waiting for Paused state: {}, current={:?}", e, curr);
                                        break;
                                    }
                                }

                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }

                            if !paused_ok {
                                error!("Failed to reach Paused state, skipping playback");
                                continue;
                            }

                            // 再生を開始
                            if let Err(e) = pipeline.set_state(gst::State::Playing) {
                                error!("Failed to set pipeline to Playing state: {}", e);
                                continue;
                            }
                            info!("Set pipeline state to Playing, waiting for transition...");

                            // Playing状態への遷移を待つ（AsyncDoneメッセージを待つ）
                            let state_change_timeout = Instant::now();
                            let mut has_error = false;
                            let mut playing_ok = false;
                            loop {
                                // busメッセージをチェック
                                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
                                    use gst::MessageView;
                                    match msg.view() {
                                        MessageView::AsyncDone(_) => {
                                            info!("AsyncDone received for Playing state");
                                        }
                                        MessageView::Error(err) => {
                                            error!(
                                                "GStreamer error during state change: {}, Debug: {:?}",
                                                err.error(),
                                                err.debug()
                                            );
                                            has_error = true;
                                            break;
                                        }
                                        MessageView::Warning(warn) => {
                                            warn!(
                                                "GStreamer warning during state change: {}, Debug: {:?}",
                                                warn.error(),
                                                warn.debug()
                                            );
                                        }
                                        MessageView::StateChanged(sc) => {
                                            if sc.src().map(|s| s == &pipeline).unwrap_or(false) {
                                                info!(
                                                    "Pipeline state changed: {:?} -> {:?}",
                                                    sc.old(),
                                                    sc.current()
                                                );
                                            }
                                        }
                                        _ => {}
                                    }
                                }

                                if has_error {
                                    error!("Breaking due to GStreamer error");
                                    break;
                                }

                                // 状態を確認
                                let elapsed = Instant::now().duration_since(state_change_timeout);
                                let (ret, current, pending) = pipeline.state(gst::ClockTime::ZERO);
                                match (ret, current, pending) {
                                    (Ok(_), gst::State::Playing, gst::State::VoidPending) => {
                                        info!("Pipeline successfully transitioned to Playing state (elapsed: {:?})", elapsed);
                                        playing_ok = true;
                                        break;
                                    }
                                    (Ok(_), curr, pend) => {
                                        debug!(
                                            "State transition in progress, current: {:?}, pending: {:?}, elapsed: {:?}",
                                            curr,
                                            pend,
                                            elapsed
                                        );
                                        if elapsed > std::time::Duration::from_secs(10) {
                                            error!("Timeout waiting for Playing state after {:?}", elapsed);
                                            break;
                                        }
                                    }
                                    (Err(e), curr, _) => {
                                        error!("Error waiting for Playing state: {}, current={:?}", e, curr);
                                        break;
                                    }
                                }

                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }

                            if !playing_ok {
                                error!("Failed to reach Playing state");
                            }

                            // 最終的な状態を確認
                            let (_, final_state, _) = pipeline.state(gst::ClockTime::ZERO);
                            info!("Final pipeline state after transition attempt: {:?}", final_state);

                            current_sound = Some(new_sound.clone());
                            // 時間の基準点をリセット
                            playback_start_time = Instant::now();
                            initial_server_time_ns = 0;
                            info!("Sound switched to {}. Continuing playback.", new_sound);
                        }
                    }
                } else {
                    let default_sound = "tsukimi-main.mp3";
                    if current_sound.as_deref() != Some(default_sound) {
                        info!("No device detected. Playing default sound: {}", default_sound);

                        // パイプラインを停止
                        if let Err(e) = pipeline.set_state(gst::State::Null) {
                            error!("Failed to set pipeline to Null state: {}", e);
                            continue;
                        }

                        // Null状態への遷移を待つ
                        let _ = pipeline.state(gst::ClockTime::from_seconds(5));

                        // ファイルパスを変更
                        filesrc.set_property("location", default_sound);

                        // 再生を開始
                        if let Err(e) = pipeline.set_state(gst::State::Playing) {
                            error!("Failed to set pipeline to Playing state: {}", e);
                            continue;
                        }

                        // Playing状態への遷移を待つ
                        let _ = pipeline.state(gst::ClockTime::from_seconds(5));

                        current_sound = Some(default_sound.to_string());
                        // 時間の基準点をリセット
                        playback_start_time = Instant::now();
                        initial_server_time_ns = 0;
                        info!("Switched to default sound. Continuing playback.");
                    }
                }

                if let Some(sound) = &current_sound {
                    if let Some(position) = pipeline.query_position::<gst::ClockTime>() {
                        info!(
                            current_sound = sound,
                            playback_time_ms = position.mseconds(),
                            "Current playback status"
                        );
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    pipeline.set_state(gst::State::Null)?;
    info!("Audio system main loop finished.");
    Ok(())
}

#[instrument(skip(volume_element, sound_setting), fields(device_address = %device_info.address, rssi = device_info.rssi))]
fn update_volume_from_rssi(
    device_info: &Arc<DeviceInfo>,
    volume_element: &gst::Element,
    sound_setting: &Arc<Mutex<SoundSetting>>,
) {
    let setting = sound_setting.lock().unwrap();
    let volume_level = get_volume_from_rssi(device_info, &setting);
    debug!(volume = volume_level, "Update volume");
    volume_element.set_property("volume", volume_level);
}

fn get_volume_from_rssi(device_info: &Arc<DeviceInfo>, setting: &SoundSetting) -> f64 {
    if setting.is_muted {
        return setting.min_volume;
    }
    let rssi = device_info.rssi as f64;
    let max_rssi = setting.max_volume_rssi;
    let min_rssi = setting.min_volume_rssi;
    if rssi >= max_rssi {
        return setting.max_volume;
    }
    if rssi <= min_rssi {
        return setting.min_volume;
    }
    let volume_ratio = (rssi - min_rssi) / (max_rssi - min_rssi);
    let volume_range = setting.max_volume - setting.min_volume;
    let volume_level = setting.min_volume + (volume_ratio * volume_range);
    volume_level.clamp(setting.min_volume, setting.max_volume)
}