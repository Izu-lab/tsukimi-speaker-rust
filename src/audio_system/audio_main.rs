use crate::proto::proto::SoundSetting;
use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use glib::object::ObjectExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn};

// 再生状態を管理するためのenum
enum PlaybackState {
    WaitingForFirstSync,
    Playing,
}

fn build_pipeline(sound_path: &str) -> Result<(gst::Pipeline, gst::Bus, Option<gst::Element>, gst::Element, gst::Element)> {
    // pitch要素を含むパイプラインを構築
    let pipeline_str = format!(
        "filesrc name=src location={} ! decodebin ! volume name=vol ! audioconvert ! capsfilter caps=\"audio/x-raw,format=F32LE,rate=44100,channels=2\" ! pitch name=pch ! audioconvert ! audioresample ! queue ! autoaudiosink",
        sound_path
    );
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Failed to downcast to Pipeline"))?;

    let bus = pipeline
        .bus()
        .ok_or_else(|| anyhow!("Failed to get bus from pipeline"))?;

    let filesrc = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow!("filesrc not found"))?;
    let volume = pipeline
        .by_name("vol")
        .ok_or_else(|| anyhow!("volume not found"))?;
    let pitch = pipeline.by_name("pch");

    Ok((pipeline, bus, pitch, filesrc, volume))
}

fn wait_for_state(pipeline: &gst::Pipeline, target: gst::State, timeout: Duration, label: &str) -> bool {
    let start = Instant::now();
    loop {
        if Instant::now().duration_since(start) > timeout {
            error!(?target, label, "Timeout waiting for state");
            return false;
        }
        let (ret, current, pending) = pipeline.state(gst::ClockTime::from_mseconds(0));
        match (ret, current, pending) {
            (Ok(_), c, gst::State::VoidPending) if c == target => {
                info!(?target, label, "Reached target state");
                return true;
            }
            (Ok(_), c, p) => {
                debug!(?c, ?p, label, "Waiting for state");
            }
            (Err(e), c, p) => {
                error!(?e, ?c, ?p, label, "Error while waiting for state");
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn seek_to_server_time(pipeline: &gst::Pipeline, bus: &gst::Bus, server_time_ns: u64) -> Result<()> {
    // durationが未決定なことがあるため、短時間ポーリングして取得を待つ
    let start = Instant::now();
    let timeout = Duration::from_secs(3);
    loop {
        if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
            if duration.nseconds() > 0 {
                let seek_time_ns = server_time_ns % duration.nseconds();
                let seek_time = gst::ClockTime::from_nseconds(seek_time_ns);
                pipeline.seek_simple(gst::SeekFlags::FLUSH, seek_time)?;
                if let Some(_) = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]) {
                    info!(?seek_time, "Seek completed (AsyncDone)");
                } else {
                    warn!(?seek_time, "AsyncDone not received after seek");
                }
                return Ok(());
            }
        }
        if Instant::now().duration_since(start) > timeout {
            warn!("Duration unavailable for seek (timeout)");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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

    // 準備
    let mut playback_state = PlaybackState::WaitingForFirstSync;
    let default_sound = "tsukimi-main.mp3".to_string();
    let mut current_sound: String = default_sound.clone();
    let mut detected_devices: HashMap<String, Arc<DeviceInfo>> = HashMap::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

    // 最初のパイプラインを構築
    let (mut pipeline, mut bus, mut pitch, mut filesrc, _volume) = build_pipeline(&current_sound)?;
    pipeline.set_state(gst::State::Paused)?;
    wait_for_state(&pipeline, gst::State::Paused, Duration::from_secs(10), "initial_pause");

    // 同期関連
    let mut current_rate = 1.0f64;
    let mut playback_start_time = Instant::now(); // 再生開始/直近seekの瞬間の実時間
    let mut initial_server_time_ns = 0u64; // ↑の瞬間のサーバー時間
    let mut last_server_time_ns: Option<u64> = None; // 直近で受信したサーバー時間

    // 時刻同期待機のタイムアウト（必要なら延長可）
    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    // バスのEOS/エラー処理はメインループ内で実施
    'main_loop: loop {
        // --- GStreamerメッセージ処理 ---
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => {
                    info!("End-of-stream reached. Looping playback.");
                    if let Err(e) = pipeline.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::from_seconds(0)) {
                        error!(%e, "Failed to seek to beginning for looping");
                    }
                }
                MessageView::Error(err) => {
                    error!(
                        error = %err.error(),
                        debug = ?err.debug(),
                        src = ?err.src().map(|s| s.name()),
                        "GStreamer pipeline error"
                    );
                    break 'main_loop;
                }
                MessageView::Warning(warn_msg) => {
                    warn!(
                        warn = %warn_msg.error(),
                        debug = ?warn_msg.debug(),
                        "GStreamer pipeline warning"
                    );
                }
                MessageView::StateChanged(state_changed) => {
                    if state_changed.src().map(|s| s == &pipeline).unwrap_or(false) {
                        debug!(old = ?state_changed.old(), current = ?state_changed.current(), "Pipeline state changed");
                    }
                }
                _ => {}
            }
        }

        // 最新のサーバー時間を可能な限り吸い上げる
        while let Ok(t) = time_sync_rx.try_recv() {
            last_server_time_ns = Some(t);
        }

        // --- 再生状態に応じた処理 ---
        match playback_state {
            PlaybackState::WaitingForFirstSync => {
                if last_server_time_ns.is_some() {
                    let server_time_ns = last_server_time_ns.unwrap();
                    // 再生開始前にシーク
                    if let Err(e) = seek_to_server_time(&pipeline, &bus, server_time_ns) {
                        warn!(%e, "Seek before first play failed");
                    }
                    pipeline.set_state(gst::State::Playing)?;
                    info!("Pipeline state set to Playing after first sync");
                    playback_start_time = Instant::now();
                    initial_server_time_ns = server_time_ns;
                    playback_state = PlaybackState::Playing;
                } else if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    // サーバー時間が来ない場合にフォールバック
                    warn!("Time sync timeout. Starting playback without sync.");
                    pipeline.set_state(gst::State::Playing)?;
                    playback_start_time = Instant::now();
                    initial_server_time_ns = 0;
                    playback_state = PlaybackState::Playing;
                }
            }
            PlaybackState::Playing => {
                // 設定更新
                if let Ok(new_setting) = sound_setting_rx.try_recv() {
                    info!(?new_setting, "Received new sound setting");
                    *sound_setting.lock().unwrap() = new_setting;
                }
                // デバイス一覧更新
                while let Ok(device_info) = rx.try_recv() {
                    detected_devices.insert(device_info.address.clone(), device_info);
                }
                // 古いデバイスのクリーンアップ
                if Instant::now().duration_since(last_cleanup) > CLEANUP_INTERVAL {
                    let initial_count = detected_devices.len();
                    detected_devices.retain(|_, device_info| Instant::now().duration_since(device_info.last_seen) < CLEANUP_INTERVAL);
                    if initial_count != detected_devices.len() {
                        debug!("Cleaned up old devices.");
                    }
                    last_cleanup = Instant::now();
                }

                // ドリフト補正
                if let Some(server_time_ns) = last_server_time_ns {
                    if initial_server_time_ns != 0 {
                        let server_elapsed = (server_time_ns - initial_server_time_ns) as i64;
                        let client_elapsed = playback_start_time.elapsed().as_nanos() as i64;
                        let diff_real_ns = server_elapsed - client_elapsed;
                        let diff_abs_s = (diff_real_ns.abs() as f64) / 1e9;

                        let new_rate: f64 = if diff_abs_s > 1.0 {
                            warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected, seeking.");
                            if let Err(e) = seek_to_server_time(&pipeline, &bus, server_time_ns) {
                                error!(%e, "Seek failed during drift correction");
                            }
                            playback_start_time = Instant::now();
                            initial_server_time_ns = server_time_ns;
                            1.0
                        } else {
                            let diff_s = diff_real_ns as f64 / 1e9;
                            const CORRECTION_TIME_S: f64 = 2.0;
                            let correction_rate = diff_s / CORRECTION_TIME_S;
                            (1.0 + correction_rate).clamp(0.9, 1.1)
                        };

                        if (new_rate - current_rate).abs() > 1e-6 {
                            if let Some(ref p) = pitch {
                                // tempo プロパティで速度補正
                                p.set_property("tempo", new_rate as f32);
                            } else {
                                warn!("pitch element not found; skipping tempo update");
                            }
                            current_rate = new_rate;
                        }
                    }
                }

                // ベストデバイス選定
                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    let my_addr_opt_clone = my_address.lock().unwrap().clone();
                    let points = *current_points.lock().unwrap();

                    let mut candidates: Vec<_> = detected_devices
                        .values()
                        .filter(|d| sound_map.contains_key(&d.address))
                        .collect();

                    candidates.sort_by(|a, b| {
                        let a_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                        let b_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                        b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                    });

                    candidates.first().cloned()
                };

                // 望ましいサウンドを決定
                let desired_sound = if let Some(device) = best_device {
                    let sound_map = sound_map.lock().unwrap();
                    sound_map.get(&device.address).cloned().unwrap_or_else(|| default_sound.clone())
                } else {
                    default_sound.clone()
                };

                // 音源切替（サーバー時間で同期）
                if desired_sound != current_sound {
                    info!(from = %current_sound, to = %desired_sound, "Switching sound (in-place)");

                    // 事前チェック
                    if !desired_sound.is_empty() {
                        let sound_path = std::path::Path::new(&desired_sound);
                        if !sound_path.exists() {
                            error!("Sound file does not exist: {}", desired_sound);
                            continue;
                        }
                    } else {
                        warn!("Sound file name is empty, skipping switch.");
                        continue;
                    }

                    // 一時停止（Nullにしない）
                    if let Err(e) = pipeline.set_state(gst::State::Paused) {
                        error!(%e, "Failed to pause pipeline for switch");
                        continue;
                    }
                    wait_for_state(&pipeline, gst::State::Paused, Duration::from_secs(5), "switch_to_paused");

                    // ロケーション更新
                    filesrc.set_property("location", &desired_sound);
                    info!("filesrc location updated");

                    // サーバー時間にシーク（duration更新を待ちながら）
                    if let Some(server_time_ns) = last_server_time_ns {
                        let _ = seek_to_server_time(&pipeline, &bus, server_time_ns);
                        playback_start_time = Instant::now();
                        initial_server_time_ns = server_time_ns;
                        current_rate = 1.0;
                    } else {
                        playback_start_time = Instant::now();
                        initial_server_time_ns = 0;
                        current_rate = 1.0;
                    }

                    // 再生再開
                    if let Err(e) = pipeline.set_state(gst::State::Playing) {
                        error!(%e, "Failed to resume Playing after switch");
                        continue;
                    }

                    current_sound = desired_sound;
                }
            }
        }
    }

    // 終了処理
    let _ = pipeline.set_state(gst::State::Null);
    Ok(())
}
