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

// 音源切り替えリクエスト
struct SwitchRequest {
    desired_sound: String,
    seek_position_ns: u64,
}

// 再生状態を管理するためのenum
enum PlaybackState {
    WaitingForFirstSync,
    Playing,
}

struct PipelineState {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    pitch: Option<gst::Element>,
    volume: gst::Element,
}

fn sink_name() -> &'static str {
    #[cfg(target_os = "linux")]
    { "pulsesink" }
    #[cfg(not(target_os = "linux"))]
    { "autoaudiosink" }
}

fn build_pipeline(sound_path: &str) -> Result<PipelineState> {
    let sink = sink_name();
    let pipeline_str = format!(
        "filesrc name=src location={} ! decodebin ! volume name=vol ! audioconvert ! capsfilter caps=\"audio/x-raw,format=F32LE,rate=44100,channels=2\" ! pitch name=pch ! audioconvert ! audioresample ! queue2 max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 use-buffering=true ! {}",
        sound_path,
        sink
    );
    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Failed to downcast to Pipeline"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("Failed to get bus from pipeline"))?;
    let volume = pipeline.by_name("vol").ok_or_else(|| anyhow!("volume not found"))?;
    let pitch = pipeline.by_name("pch");
    Ok(PipelineState { pipeline, bus, pitch, volume })
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
                debug!(?target, label, "Reached target state");
                return true;
            }
            (Ok(_), _c, _p) => {
                // 状態遷移中、ポーリング間隔を短縮
            }
            (Err(e), c, p) => {
                error!(?e, ?c, ?p, label, "Error while waiting for state");
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(20)); // 50ms → 20ms に短縮
    }
}

fn seek_to_server_time(pipeline: &gst::Pipeline, bus: &gst::Bus, server_time_ns: u64) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(3);
    loop {
        if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
            if duration.nseconds() > 0 {
                let seek_time_ns = server_time_ns % duration.nseconds();
                let seek_time = gst::ClockTime::from_nseconds(seek_time_ns);
                pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, seek_time)?;
                if let Some(_) = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]) {
                    debug!(?seek_time, "Seek completed");
                    // FLUSHシーク後の待機時間を短縮
                    std::thread::sleep(Duration::from_millis(50)); // 100ms → 50ms
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
        std::thread::sleep(Duration::from_millis(20)); // 50ms → 20ms
    }
}

fn set_volume(volume: &gst::Element, v: f64) {
    volume.set_property("volume", v);
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
        max_volume_rssi: 0.0,
        min_volume_rssi: 0.0,
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

    // アクティブ/インアクティブの2系統を保持
    let mut active: Option<PipelineState> = None;
    let mut standby: Option<PipelineState> = None;

    // 音源切り替え用のチャネル
    let (switch_tx, mut switch_rx) = mpsc::channel::<PipelineState>(1);

    // 同期関連
    let mut playback_start_time = Instant::now();
    let mut initial_server_time_ns = 0u64;
    let mut last_server_time_ns: Option<u64> = None;
    // スイッチング中/直後のシーク抑止用ガード
    let mut switching = false;
    let mut last_switch_end: Option<Instant> = None;
    const SWITCH_GUARD_WINDOW: Duration = Duration::from_millis(400);

    // 独自のシーク位置管理
    let mut current_seek_position_ns: u64 = 0;
    let mut last_position_update = Instant::now();

    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    'main_loop: loop {
        // バス処理（アクティブ優先、スタンバイも確認）- タイムアウトを短縮
        if let Some(ref act) = active {
            while let Some(msg) = act.bus.timed_pop(gst::ClockTime::from_mseconds(1)) { // 5ms → 1ms
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(_) => {
                        info!("Active pipeline EOS, looping");
                        let _ = act.pipeline.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::from_seconds(0));
                    }
                    MessageView::Error(err) => {
                        error!(error=%err.error(), debug=?err.debug(), src=?err.src().map(|s| s.name()), "Active pipeline error");
                        break 'main_loop;
                    }
                    _ => {}
                }
            }
        }
        if let Some(ref stdb) = standby {
            while let Some(msg) = stdb.bus.timed_pop(gst::ClockTime::from_nseconds(500_000)) { // 1ms → 0.5ms
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        warn!(error=%err.error(), debug=?err.debug(), src=?err.src().map(|s| s.name()), "Standby pipeline error");
                    }
                    _ => {}
                }
            }
        }

        // 最新サーバー時間を吸い上げ
        while let Ok(t) = time_sync_rx.try_recv() { last_server_time_ns = Some(t); }

        match playback_state {
            PlaybackState::WaitingForFirstSync => {
                if let Some(server_time_ns) = last_server_time_ns {
                    // 初回アクティブを作成
                    let act = build_pipeline(&current_sound)?;
                    let _ = act.pipeline.set_state(gst::State::Paused);
                    wait_for_state(&act.pipeline, gst::State::Paused, Duration::from_secs(10), "initial_pause");
                    let _ = seek_to_server_time(&act.pipeline, &act.bus, server_time_ns);
                    if let Some(ref p) = act.pitch { p.set_property("tempo", 1.0f32); }
                    set_volume(&act.volume, 1.0);
                    let _ = act.pipeline.set_state(gst::State::Playing);
                    active = Some(act);

                    // 独自シーク位置を初期化
                    if let Some(duration) = active.as_ref().and_then(|a| a.pipeline.query_duration::<gst::ClockTime>()) {
                        current_seek_position_ns = server_time_ns % duration.nseconds();
                    }
                    last_position_update = Instant::now();

                    playback_start_time = Instant::now();
                    initial_server_time_ns = server_time_ns;
                    playback_state = PlaybackState::Playing;
                } else if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    // 同期なしフォールバック
                    let act = build_pipeline(&current_sound)?;
                    let _ = act.pipeline.set_state(gst::State::Playing);
                    set_volume(&act.volume, 1.0);
                    active = Some(act);

                    current_seek_position_ns = 0;
                    last_position_update = Instant::now();

                    playback_start_time = Instant::now();
                    initial_server_time_ns = 0;
                    playback_state = PlaybackState::Playing;
                }
            }
            PlaybackState::Playing => {
                // 独自シーク位置を経過時間で更新
                let elapsed_since_update = last_position_update.elapsed();
                current_seek_position_ns += elapsed_since_update.as_nanos() as u64;
                last_position_update = Instant::now();

                // 音源の長さでループ（必要に応じて）
                if let Some(ref act) = active {
                    if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                        if duration.nseconds() > 0 {
                            current_seek_position_ns %= duration.nseconds();
                        }
                    }
                }

                // 設定更新
                if let Ok(new_setting) = sound_setting_rx.try_recv() {
                    info!(?new_setting, "Received new sound setting");
                    *sound_setting.lock().unwrap() = new_setting;
                }
                // デバイス更新
                while let Ok(device_info) = rx.try_recv() {
                    detected_devices.insert(device_info.address.clone(), device_info);
                }
                if Instant::now().duration_since(last_cleanup) > CLEANUP_INTERVAL {
                    let initial_count = detected_devices.len();
                    detected_devices.retain(|_, d| Instant::now().duration_since(d.last_seen) < CLEANUP_INTERVAL);
                    if initial_count != detected_devices.len() { debug!("Cleaned up old devices."); }
                    last_cleanup = Instant::now();
                }

                // ドリフト補正（アクティブ側のみ）
                if let (Some(server_time_ns), Some(ref act)) = (last_server_time_ns, active.as_ref()) {
                    // 切替中と直後のウィンドウはシークを行わない
                    let in_switch_guard = switching || last_switch_end.map_or(false, |t| Instant::now().duration_since(t) < SWITCH_GUARD_WINDOW);
                    if initial_server_time_ns != 0 && !in_switch_guard {
                        let server_elapsed = (server_time_ns - initial_server_time_ns) as i64;
                        let client_elapsed = playback_start_time.elapsed().as_nanos() as i64;
                        let diff_real_ns = server_elapsed - client_elapsed;
                        let diff_abs_s = (diff_real_ns.abs() as f64) / 1e9;
                        let new_rate: f64 = if diff_abs_s > 3.0 {
                            warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected (>3s), seeking active.");
                            let _ = seek_to_server_time(&act.pipeline, &act.bus, server_time_ns);
                            // 独自シーク位置も更新
                            if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                                if duration.nseconds() > 0 {
                                    current_seek_position_ns = server_time_ns % duration.nseconds();
                                }
                            }
                            1.0
                        } else {
                            let diff_s = diff_real_ns as f64 / 1e9;
                            const CORRECTION_TIME_S: f64 = 2.0;
                            (1.0 + diff_s / CORRECTION_TIME_S).clamp(0.9, 1.1)
                        };
                        if let Some(ref p) = act.pitch { p.set_property("tempo", new_rate as f32); }
                        playback_start_time = Instant::now();
                        initial_server_time_ns = server_time_ns;
                    }
                }

                // ベストデバイス選定
                // RSSI閾値: この値を超えたデバイスのみが候補になる
                const RSSI_THRESHOLD: i16 = -70;

                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    let my_addr_opt_clone = my_address.lock().unwrap().clone();
                    let points = *current_points.lock().unwrap();

                    // RSSI閾値を超えたデバイスのみをフィルタリング
                    let mut candidates: Vec<_> = detected_devices.values()
                        .filter(|d| sound_map.contains_key(&d.address) && d.rssi > RSSI_THRESHOLD)
                        .collect();

                    // ポイント優先、同じポイントならRSSI優先でソート
                    candidates.sort_by(|a, b| {
                        let a_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                        let b_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                        b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                    });

                    candidates.first().cloned()
                };

                // 全てのLocationが閾値を下回っているかチェック
                let all_below_threshold = {
                    let sound_map = sound_map.lock().unwrap();

                    // 現在再生中のサウンドに対応するデバイスを探す
                    let current_sound_device = detected_devices.values()
                        .find(|d| {
                            sound_map.get(&d.address)
                                .map(|sound| sound == &current_sound)
                                .unwrap_or(false)
                        });

                    // 現在再生中のサウンドに対応するデバイスが閾値を下回っているかチェック
                    if let Some(device) = current_sound_device {
                        // 現在のデバイスが閾値を下回っている場合のみtrue
                        device.rssi <= RSSI_THRESHOLD
                    } else if current_sound == default_sound {
                        // すでにデフォルトサウンドの場合、閾値チェックをスキップ
                        false
                    } else {
                        // 現在のサウンドに対応するデバイスが見つからない場合（通信途絶など）
                        // 登録されている全デバイスが閾値を下回っているかチェック
                        let registered_devices: Vec<_> = detected_devices.values()
                            .filter(|d| sound_map.contains_key(&d.address))
                            .collect();

                        !registered_devices.is_empty() && registered_devices.iter().all(|d| d.rssi <= RSSI_THRESHOLD)
                    }
                };

                let desired_sound = if let Some(device) = best_device {
                    // 閾値を超えたデバイスがある場合、そのデバイスのサウンドを使用
                    let sound_map = sound_map.lock().unwrap();
                    sound_map.get(&device.address).cloned().unwrap_or_else(|| current_sound.clone())
                } else if all_below_threshold {
                    // 現在のLocationが閾値を下回った場合のみデフォルトサウンドに切り替え
                    default_sound.clone()
                } else {
                    // それ以外の場合（デバイスが検出されていない等）は現在のサウンドを維持
                    current_sound.clone()
                };

                // 非同期切り替えの完了チェック
                if let Ok(new_pipeline) = switch_rx.try_recv() {
                    info!("✅ 非同期切り替え完了、新パイプラインを適用");

                    // 旧パイプラインを停止
                    if let Some(old) = active.take() {
                        let _ = old.pipeline.set_state(gst::State::Null);
                    }

                    // 新パイプラインをアクティブに
                    active = Some(new_pipeline);

                    // 同期を再設定
                    last_position_update = Instant::now();
                    playback_start_time = Instant::now();
                    if let Some(t) = last_server_time_ns {
                        initial_server_time_ns = t;
                    }

                    switching = false;
                    last_switch_end = Some(Instant::now());
                    info!("🎉 音源切り替え完了");
                }

                // 音源切り替えリクエスト処理
                if desired_sound != current_sound && !switching {
                    info!(from=%current_sound, to=%desired_sound, "🔄 音源切り替えリクエスト送信");
                    switching = true;
                    current_sound = desired_sound.clone();

                    // スタンバイパイプラインがあれば停止して破棄
                    if let Some(old_standby) = standby.take() {
                        let _ = old_standby.pipeline.set_state(gst::State::Null);
                    }

                    // 非同期切り替えリクエストを送信
                    let request = SwitchRequest {
                        desired_sound: desired_sound.clone(),
                        seek_position_ns: current_seek_position_ns,
                    };

                    let switch_tx_clone = switch_tx.clone();

                    // 別スレッドで切り替え処理を実行
                    std::thread::spawn(move || {
                        info!("📦 非同期で新しいパイプラインを構築中...");

                        match build_pipeline(&request.desired_sound) {
                            Ok(next) => {
                                // volume=1.0で設定
                                set_volume(&next.volume, 1.0);
                                if let Some(ref p) = next.pitch {
                                    p.set_property("tempo", 1.0f32);
                                }

                                // Paused状態でシーク
                                info!("⏸️  Paused状態で独自シーク位置 {} ns にシーク", request.seek_position_ns);
                                let _ = next.pipeline.set_state(gst::State::Paused);
                                wait_for_state(&next.pipeline, gst::State::Paused, Duration::from_secs(3), "async_switch_pause");

                                let seek_position = gst::ClockTime::from_nseconds(request.seek_position_ns);
                                let _ = next.pipeline.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
                                    seek_position
                                );
                                let _ = next.bus.timed_pop_filtered(
                                    Some(gst::ClockTime::from_mseconds(500)),
                                    &[gst::MessageType::AsyncDone]
                                );
                                info!("✓ シーク完了");

                                // Playing状態に遷移
                                info!("▶️  パイプラインをPlaying状態に設定");
                                let _ = next.pipeline.set_state(gst::State::Playing);

                                // 完成したパイプラインをメインスレッドに送信
                                if let Err(e) = switch_tx_clone.blocking_send(next) {
                                    error!("Failed to send new pipeline: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Failed to build pipeline: {}", e);
                            }
                        }
                    });
                }
            }
        }

        // メインループの sleep を削除し、代わりに短い待機のみ
        // イベント駆動型にすることで、レスポンス性を向上
        std::thread::sleep(Duration::from_millis(1)); // 最小限の待機のみ
    }

    // 終了処理
    if let Some(act) = active { let _ = act.pipeline.set_state(gst::State::Null); }
    if let Some(st) = standby { let _ = st.pipeline.set_state(gst::State::Null); }
    Ok(())
}
