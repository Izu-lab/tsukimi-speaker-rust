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



    let mut current_sound = None::<String>;
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
    info!("Pipeline state set to Paused. Waiting for first time sync...");

    let bus = pipeline.bus().unwrap();

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
                    error!("GStreamer pipeline error: {}, Debug info: {:?}", err.error(), err.debug());
                    break 'main_loop;
                }
                _ => (),
            }
        }

        // --- 再生状態に応じた処理 ---
        match playback_state {
            PlaybackState::WaitingForFirstSync => {
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
                if let Ok(server_time_ns) = time_sync_rx.try_recv() {
                    // --- 実時間ベースの同期ロジック ---
                    let server_elapsed = (server_time_ns - initial_server_time_ns) as i64;
                    let client_elapsed = playback_start_time.elapsed().as_nanos() as i64;
                    let diff_real_ns = server_elapsed - client_elapsed;
                    let diff_abs_s = (diff_real_ns.abs() as f64) / 1e9;

                    let mut new_rate = current_rate;

                    if diff_abs_s > 1.0 {
                        warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected, seeking.");
                        let duration = pipeline.query_duration::<gst::ClockTime>().unwrap();
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
            }
        }

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
            let best_device = {
                let sound_map = sound_map.lock().unwrap();
                let my_addr_opt_clone = my_address.lock().unwrap().clone();
                let points = *current_points.lock().unwrap();

                let mut candidates: Vec<_> = detected_devices
                    .values()
                    .filter(|d| sound_map.contains_key(&d.address))
                    .collect();

                // ポイントとRSSIでソート
                // 1. ポイントが高い順 (自分自身のデバイスであれば現在のポイント、そうでなければ0)
                // 2. RSSIが高い順
                candidates.sort_by(|a, b| {
                    let a_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                    let b_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                    b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                });

                candidates.first().cloned()
            };
        if let Some(device) = best_device {
            let sound_map = sound_map.lock().unwrap();
            if let Some(new_sound) = sound_map.get(&device.address) {
                if current_sound.as_deref() != Some(new_sound.as_str()) {
                    info!("Switching sound to {}", new_sound);
                    pipeline.set_state(gst::State::Null)?;
                    filesrc.set_property("location", new_sound.clone());
                    pipeline.set_state(gst::State::Playing)?;
                    current_sound = Some(new_sound.clone());
                }
            }
        } else {
            let default_sound = "tsukimi-main.mp3";
            if current_sound.as_deref() != Some(default_sound) {
                info!("No device detected. Playing default sound: {}", default_sound);
                pipeline.set_state(gst::State::Null)?;
                filesrc.set_property("location", default_sound);
                pipeline.set_state(gst::State::Playing)?;
                current_sound = Some(default_sound.to_string());
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