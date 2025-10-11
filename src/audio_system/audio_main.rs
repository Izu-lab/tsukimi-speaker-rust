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

// SE再生リクエスト
#[derive(Debug, Clone)]
pub struct SePlayRequest {
    pub file_path: String,
}

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
    // ファイルの存在確認
    if !std::path::Path::new(sound_path).exists() {
        return Err(anyhow!("Audio file not found: {}", sound_path));
    }

    let sink = sink_name();
    let pipeline_str = format!(
        "filesrc name=src location={} ! decodebin ! volume name=vol ! audioconvert ! capsfilter caps=\"audio/x-raw,format=F32LE,rate=44100,channels=2\" ! pitch name=pch ! audioconvert ! audioresample ! queue2 max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 use-buffering=true ! {}",
        sound_path,
        sink
    );

    debug!("Building pipeline: {}", pipeline_str);

    let pipeline = gst::parse::launch(&pipeline_str)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Failed to downcast to Pipeline"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("Failed to get bus from pipeline"))?;
    let volume = pipeline.by_name("vol").ok_or_else(|| anyhow!("volume not found"))?;
    let pitch = pipeline.by_name("pch");

    // バスからエラーメッセージをチェック
    if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error]) {
        if let gst::MessageView::Error(err) = msg.view() {
            return Err(anyhow!("Pipeline error: {} (debug: {:?})", err.error(), err.debug()));
        }
    }

    Ok(PipelineState { pipeline, bus, pitch, volume })
}

fn wait_for_state(pipeline: &gst::Pipeline, target: gst::State, timeout: Duration, label: &str) -> bool {
    let start = Instant::now();
    let bus = pipeline.bus();

    loop {
        if Instant::now().duration_since(start) > timeout {
            error!(?target, label, "Timeout waiting for state");

            // バスからエラーメッセージを確認
            if let Some(bus) = &bus {
                while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning]) {
                    match msg.view() {
                        gst::MessageView::Error(err) => {
                            error!("Pipeline error: {} (debug: {:?})", err.error(), err.debug());
                        }
                        gst::MessageView::Warning(warn) => {
                            warn!("Pipeline warning: {} (debug: {:?})", warn.error(), warn.debug());
                        }
                        _ => {}
                    }
                }
            }
            return false;
        }

        // バスからエラーメッセージをチェック
        if let Some(bus) = &bus {
            if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::ZERO, &[gst::MessageType::Error]) {
                if let gst::MessageView::Error(err) = msg.view() {
                    error!("Pipeline error during state change: {} (debug: {:?})", err.error(), err.debug());
                    return false;
                }
            }
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

                // バスからエラーメッセージを確認
                if let Some(bus) = &bus {
                    while let Some(msg) = bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning]) {
                        match msg.view() {
                            gst::MessageView::Error(err) => {
                                error!("Pipeline error: {} (debug: {:?})", err.error(), err.debug());
                            }
                            gst::MessageView::Warning(warn) => {
                                warn!("Pipeline warning: {} (debug: {:?})", warn.error(), warn.debug());
                            }
                            _ => {}
                        }
                    }
                }
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

#[instrument(skip(rx, time_sync_rx, sound_map, se_rx, system_enabled_rx))]
pub fn audio_main(
    mut rx: broadcast::Receiver<Arc<DeviceInfo>>,
    mut time_sync_rx: mpsc::Receiver<u64>,
    mut sound_setting_rx: mpsc::Receiver<SoundSetting>,
    mut se_rx: mpsc::Receiver<SePlayRequest>,
    mut system_enabled_rx: broadcast::Receiver<crate::connect_system::connect_main::SystemEnabledState>,
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

    // システム有効化状態を追跡
    let mut system_enabled = true;

    gst::init()?;
    info!("GStreamer initialized successfully.");

    // 準備
    let mut playback_state = PlaybackState::WaitingForFirstSync;
    let default_sound = "tsukimi-main_1.mp3".to_string();
    let mut current_sound: String = default_sound.clone();
    let mut detected_devices: HashMap<String, Arc<DeviceInfo>> = HashMap::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

    // アクティブ/インアクティブの2系統を保持
    let mut active: Option<PipelineState> = None;
    let mut standby: Option<PipelineState> = None;

    // SE再生用のパイプライン（独立して管理）
    let mut se_pipeline: Option<gst::Pipeline> = None;

    // SE再生中フラグ（音源切り替え時の音量管理に使用）
    let mut is_se_playing = false;

    // システム有効化時のSE再生フラグ
    let mut should_play_activation_se = false;

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

    // 最適化: durationのキャッシュ
    let mut cached_duration_ns: Option<u64> = None;
    let mut last_duration_query = Instant::now();
    const DURATION_QUERY_INTERVAL: Duration = Duration::from_secs(1);

    'main_loop: loop {
        // システム有効化状態のチェック
        if let Ok(state) = system_enabled_rx.try_recv() {
            info!(enabled = state.enabled, "System enabled state changed");
            system_enabled = state.enabled;

            if !system_enabled {
                // システムが無効化された場合、すべてのパイプラインを停止
                info!("🛑 System disabled - stopping all audio pipelines");

                if let Some(act) = active.take() {
                    let _ = act.pipeline.set_state(gst::State::Null);
                    info!("Stopped active pipeline");
                }

                if let Some(st) = standby.take() {
                    let _ = st.pipeline.set_state(gst::State::Null);
                    info!("Stopped standby pipeline");
                }

                if let Some(se) = se_pipeline.take() {
                    let _ = se.set_state(gst::State::Null);
                    info!("Stopped SE pipeline");
                }

                is_se_playing = false;

                // 再生状態を初期化に戻す
                playback_state = PlaybackState::WaitingForFirstSync;
                info!("Audio system paused, waiting for system to be re-enabled");
            } else {
                // システムが再有効化された場合
                info!("✅ System re-enabled - resuming audio system");
                playback_state = PlaybackState::WaitingForFirstSync;

                // 有効化SEを再生するフラグを立てる
                should_play_activation_se = true;
            }
        }

        // システムが無効化されている場合はスキップ
        if !system_enabled {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        // バス処理（アクティブ優先、スタンバイも確認）- タイムアウトを適切に調整
        if let Some(ref act) = active {
            // 10msに変更：メッセージ処理の余裕を持たせる
            while let Some(msg) = act.bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
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
                    MessageView::Buffering(buffering_msg) => {
                        let percent = buffering_msg.percent();
                        if percent < 100 {
                            debug!(?percent, "Pipeline buffering");
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(ref stdb) = standby {
            // スタンバイは1msで十分
            while let Some(msg) = stdb.bus.timed_pop(gst::ClockTime::from_mseconds(1)) {
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

        // システム有効化時のSE再生処理
        if should_play_activation_se && !is_se_playing {
            info!("🎵 システム有効化SE再生開始");
            should_play_activation_se = false;

            // SE再生中フラグを立てる
            is_se_playing = true;

            // 既存のSEパイプラインがあれば停止
            if let Some(old_se) = se_pipeline.take() {
                info!("🛑 既存のSEパイプラインを停止してクリーンアップ");
                let _ = old_se.set_state(gst::State::Null);
            }

            // 新しいSEパイプラインを作成（システム有効化SE）
            let sink = sink_name();
            let se_file = "se-activation.mp3"; // システム有効化音

            // PulseAudioの場合は明示的にストリーム名とclient名を設定
            let se_pipeline_str = if cfg!(target_os = "linux") {
                format!(
                    "filesrc location={} ! decodebin ! audioconvert ! audioresample ! volume name=se_vol volume=3.0 ! pulsesink client-name=\"tsukimi-se\" stream-properties=\"properties,media.role=event\"",
                    se_file
                )
            } else {
                format!(
                    "filesrc location={} ! decodebin ! audioconvert ! audioresample ! volume name=se_vol volume=3.0 ! {}",
                    se_file, sink
                )
            };

            info!("🎵 システム有効化SEパイプライン構築開始: pipeline={}", se_pipeline_str);

            match gst::parse::launch(&se_pipeline_str) {
                Ok(pipeline) => {
                    if let Ok(se_pipe) = pipeline.downcast::<gst::Pipeline>() {
                        info!("✅ システム有効化SEパイプライン作成成功");
                        info!("▶️  システム有効化SE再生開始: {}", se_file);
                        let _ = se_pipe.set_state(gst::State::Playing);
                        se_pipeline = Some(se_pipe);
                    } else {
                        error!("❌ システム有効化SEパイプラインのダウンキャストに失敗");
                        is_se_playing = false;
                    }
                }
                Err(e) => {
                    error!("❌ システム有効化SEパイプラインの構築に失敗: error={}", e);
                    is_se_playing = false;
                }
            }
        }

        // SE再生リクエストの処理
        if let Ok(se_request) = se_rx.try_recv() {
            info!("🔔 SE再生リクエスト受信: file={}", se_request.file_path);

            // SE再生中フラグを立てる
            is_se_playing = true;


            // 既存のSEパイプラインがあれば停止
            if let Some(old_se) = se_pipeline.take() {
                info!("🛑 既存のSEパイプラインを停止してクリーンアップ");
                let _ = old_se.set_state(gst::State::Null);
            }

            // 新しいSEパイプラインを作成（シンプルなワンショット再生）
            let sink = sink_name();

            // PulseAudioの場合は明示的にストリーム名とclient名を設定
            let se_pipeline_str = if cfg!(target_os = "linux") {
                format!(
                    "filesrc location={} ! decodebin ! audioconvert ! audioresample ! volume name=se_vol volume=3.0 ! pulsesink client-name=\"tsukimi-se\" stream-properties=\"properties,media.role=event\"",
                    se_request.file_path
                )
            } else {
                format!(
                    "filesrc location={} ! decodebin ! audioconvert ! audioresample ! volume name=se_vol volume=3.0 ! {}",
                    se_request.file_path, sink
                )
            };

            info!("🎵 SEパイプライン構築開始: pipeline={}", se_pipeline_str);

            match gst::parse::launch(&se_pipeline_str) {
                Ok(pipeline) => {
                    if let Ok(se_pipe) = pipeline.downcast::<gst::Pipeline>() {
                        info!("✅ SEパイプライン作成成功: file={}", se_request.file_path);
                        info!("▶️  SE再生開始: {}", se_request.file_path);
                        let _ = se_pipe.set_state(gst::State::Playing);
                        se_pipeline = Some(se_pipe);
                    } else {
                        error!("❌ SEパイプラインのダウンキャストに失敗: file={}", se_request.file_path);
                    }
                }
                Err(e) => {
                    error!("❌ SEパイプラインの構築に失敗: file={}, error={}", se_request.file_path, e);
                }
            }
        }

        // SE再生の完了チェック（EOSメッセージを確認）
        if let Some(ref se_pipe) = se_pipeline {
            if let Some(bus) = se_pipe.bus() {
                let mut should_clear = false;
                while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(1)) {
                    use gst::MessageView;
                    match msg.view() {
                        MessageView::Eos(_) => {
                            info!("🎵 SE再生完了 (EOS受信) - パイプラインを終了します");
                            should_clear = true;
                        }
                        MessageView::Error(err) => {
                            error!("❌ SEパイプラインエラー: error={}, debug={:?}", err.error(), err.debug());
                            should_clear = true;
                        }
                        MessageView::StateChanged(state_changed) => {
                            if let Some(src) = state_changed.src() {
                                if src == &se_pipe.clone().upcast::<gst::Object>() {
                                    let old = state_changed.old();
                                    let new = state_changed.current();
                                    let pending = state_changed.pending();
                                    info!("🔄 SEパイプライン状態変更: {:?} -> {:?} (pending: {:?})", old, new, pending);
                                }
                            }
                        }
                        MessageView::StreamStart(_) => {
                            info!("�� SEストリーム開始");
                        }
                        _ => {}
                    }
                }
                if should_clear {
                    info!("🧹 SEパイプラインをクリーンアップして解放");
                    if let Some(se_pipe) = se_pipeline.take() {
                        let _ = se_pipe.set_state(gst::State::Null);
                    }
                    // SE再生中フラグをリセット
                    is_se_playing = false;
                }
            }
        }

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

                    // durationをキャッシュ
                    if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                        cached_duration_ns = Some(duration.nseconds());
                        current_seek_position_ns = server_time_ns % duration.nseconds();
                    }

                    active = Some(act);
                    last_position_update = Instant::now();
                    last_duration_query = Instant::now();

                    playback_start_time = Instant::now();
                    initial_server_time_ns = server_time_ns;
                    playback_state = PlaybackState::Playing;
                } else if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    // 同期なしフォールバック
                    let act = build_pipeline(&current_sound)?;
                    let _ = act.pipeline.set_state(gst::State::Playing);
                    set_volume(&act.volume, 1.0);

                    if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                        cached_duration_ns = Some(duration.nseconds());
                    }

                    active = Some(act);

                    current_seek_position_ns = 0;
                    last_position_update = Instant::now();
                    last_duration_query = Instant::now();

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

                // durationのクエリを削減：1秒に1回のみ
                if Instant::now().duration_since(last_duration_query) > DURATION_QUERY_INTERVAL {
                    if let Some(ref act) = active {
                        if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                            if duration.nseconds() > 0 {
                                cached_duration_ns = Some(duration.nseconds());
                            }
                        }
                    }
                    last_duration_query = Instant::now();
                }

                // キャッシュされたdurationでループ
                if let Some(duration_ns) = cached_duration_ns {
                    if duration_ns > 0 {
                        current_seek_position_ns %= duration_ns;
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
                            // 独自シーク位置も更新、キャッシュされたdurationを使用
                            if let Some(duration_ns) = cached_duration_ns {
                                if duration_ns > 0 {
                                    current_seek_position_ns = server_time_ns % duration_ns;
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
                const RSSI_THRESHOLD: i16 = -70;

                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    let my_addr_opt_clone = my_address.lock().unwrap().clone();
                    let points = *current_points.lock().unwrap();

                    let mut candidates: Vec<_> = detected_devices.values()
                        .filter(|d| sound_map.contains_key(&d.address) && d.rssi > RSSI_THRESHOLD)
                        .collect();

                    candidates.sort_by(|a, b| {
                        let a_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                        let b_points = my_addr_opt_clone.as_deref().map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                        b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                    });

                    candidates.first().cloned()
                };

                let all_below_threshold = {
                    let sound_map = sound_map.lock().unwrap();

                    let current_sound_device = detected_devices.values()
                        .find(|d| {
                            sound_map.get(&d.address)
                                .map(|sound| sound == &current_sound)
                                .unwrap_or(false)
                        });

                    if let Some(device) = current_sound_device {
                        device.rssi <= RSSI_THRESHOLD
                    } else if current_sound == default_sound {
                        false
                    } else {
                        let registered_devices: Vec<_> = detected_devices.values()
                            .filter(|d| sound_map.contains_key(&d.address))
                            .collect();

                        !registered_devices.is_empty() && registered_devices.iter().all(|d| d.rssi <= RSSI_THRESHOLD)
                    }
                };

                let desired_sound = if let Some(device) = best_device {
                    let sound_map = sound_map.lock().unwrap();
                    // sound_mapには既にポイント付きファイル名が格納されている
                    let sound = sound_map.get(&device.address).cloned().unwrap_or_else(|| current_sound.clone());
                    info!(
                        device_address = %device.address,
                        device_rssi = device.rssi,
                        selected_sound = %sound,
                        "🎵 デバイスに基づいて音源を選択"
                    );
                    sound
                } else if all_below_threshold {
                    // デフォルトサウンド（既に_1.mp3形式）
                    info!(selected_sound = %default_sound, "🎵 全デバイスが閾値以下、デフォルト音源を選択");
                    default_sound.clone()
                } else {
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


                    // durationキャッシュを更新
                    if let Some(ref act) = active {
                        if let Some(duration) = act.pipeline.query_duration::<gst::ClockTime>() {
                            cached_duration_ns = Some(duration.nseconds());
                        }
                    }

                    // 同期を再設定
                    last_position_update = Instant::now();
                    last_duration_query = Instant::now();
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
                    let current_points = current_points.lock().unwrap();
                    info!(
                        from = %current_sound,
                        to = %desired_sound,
                        current_points = *current_points,
                        "🔄 音源切り替えリクエスト送信 (ポイント情報付き)"
                    );
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
                                set_volume(&next.volume, 1.0);
                                if let Some(ref p) = next.pitch {
                                    p.set_property("tempo", 1.0f32);
                                }

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

                                info!("▶️  パイプラインをPlaying状態に設定");
                                let _ = next.pipeline.set_state(gst::State::Playing);

                                // 🔥 重要：バッファリング完了を待つ
                                info!("⏳ バッファリング完了を待機中...");
                                let buffering_start = std::time::Instant::now();
                                let buffering_timeout = Duration::from_secs(5);
                                let mut is_buffered = false;
                                let mut last_percent = 0;

                                while std::time::Instant::now().duration_since(buffering_start) < buffering_timeout {
                                    // バッファリングメッセージを確認（短いタイムアウトで頻繁にチェック）
                                    while let Some(msg) = next.bus.timed_pop(gst::ClockTime::from_mseconds(50)) {
                                        use gst::MessageView;
                                        match msg.view() {
                                            MessageView::Buffering(buffering_msg) => {
                                                let percent = buffering_msg.percent();
                                                if percent != last_percent && (percent % 25 == 0 || percent >= 100) {
                                                    info!("��� バッファリング進行: {}%", percent);
                                                    last_percent = percent;
                                                }
                                                if percent >= 100 {
                                                    is_buffered = true;
                                                    info!("✅ バッファリング完了 (100%)");
                                                    break;
                                                }
                                            }
                                            MessageView::Error(err) => {
                                                error!("❌ 新パイプラインでエラー: {}", err.error());
                                                return;
                                            }
                                            _ => {}
                                        }
                                    }

                                    if is_buffered {
                                        break;
                                    }

                                    // まだバッファリング中の場合は少し待機
                                    std::thread::sleep(Duration::from_millis(50));
                                }

                                // バッファリングが完了していない場合でも、タイムアウト後は続行
                                if !is_buffered {
                                    warn!("⚠️  バッファリングタイムアウト、続行します");
                                } else {
                                    info!("🎵 新パイプラインの準備完了、切り替え可能");
                                }

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

        // ⚠️ 重要���sleepを完全に削除してCPU使用率を最小化しつつ、
        // バスタイムアウト(10ms)で自然な待機を実現
        // これによりGStreamerのイベント処理が滞らない
    }

    // 終了処理
    if let Some(act) = active { let _ = act.pipeline.set_state(gst::State::Null); }
    if let Some(st) = standby { let _ = st.pipeline.set_state(gst::State::Null); }
    Ok(())
}
