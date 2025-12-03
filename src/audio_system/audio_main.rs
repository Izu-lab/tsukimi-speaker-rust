use crate::proto::proto::SoundSetting;
use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use glib::object::ObjectExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn};
use chrono::Local;

// SE再生リクエスト
#[derive(Debug, Clone)]
pub struct SePlayRequest {
    pub file_path: String,
}

// 音源切り替えリクエスト
struct SwitchRequest {
    desired_sound: String,
    server_time_ns: u64,
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

impl Drop for PipelineState {
    fn drop(&mut self) {
        // パイプラインをNULL状態に設定してリソースを解放する
        // これにより、PLAYING状態のまま要素が破棄されるのを防ぐ
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            warn!(pipeline_name = %self.pipeline.name(), ?err, "Failed to set pipeline to NULL state on drop.");
        }
    }
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
    // pitchプラグインの前にqueueを追加して、十分なバッファサイズを確保
    // これによりSoundTouchライブラリのFIRFilterのアサーションエラーを回避
    let pipeline_str = format!(
        "filesrc name=src location={} ! decodebin ! audioconvert ! audioresample ! volume name=vol ! audioconvert ! capsfilter caps=\"audio/x-raw,format=F32LE,rate=44100,channels=2\" ! queue max-size-buffers=100 max-size-time=1000000000 ! pitch name=pch ! audioconvert ! audioresample ! queue2 max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 use-buffering=true ! {}",
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



#[instrument(skip(rx, time_offset, sound_map, se_rx, system_enabled_rx))]
pub fn audio_main(
    mut rx: broadcast::Receiver<Arc<DeviceInfo>>,
    time_offset: Arc<Mutex<i64>>,
    mut sound_setting_rx: mpsc::Receiver<SoundSetting>,
    mut se_rx: mpsc::Receiver<SePlayRequest>,
    mut system_enabled_rx: broadcast::Receiver<crate::connect_system::connect_main::SystemEnabledState>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
    current_location_address: Arc<Mutex<Option<String>>>,
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
    let mut last_log_time = Instant::now();
    const LOG_INTERVAL: Duration = Duration::from_secs(1);

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




    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    // 最適化: durationのキャッシュ



    const MAINTAIN_THRESHOLD_RSSI: i16 = -75; // 現在の場所にいると判断し続けるための最低RSSI

    'main_loop: loop {
        // システム有効化状態のチェック
        if let Ok(state) = system_enabled_rx.try_recv() {
            // 自分向けのイベントか確認
            let my_addr_guard = my_address.lock().unwrap();
            if my_addr_guard.as_ref() == Some(&state.target_device_id) {
                info!(enabled = state.enabled, target = %state.target_device_id, "Received SystemEnabledState for me");
                system_enabled = state.enabled;

                if !system_enabled {
                    // システムが無効化された場合、すべてのパイプラインを停止
                    info!("🛑 System disabled - stopping all audio pipelines");

                    if let Some(_act) = active.take() {
                        info!("Stopped active pipeline");
                    }

                    if let Some(_st) = standby.take() {
                        info!("Stopped standby pipeline");
                    }

                    if let Some(_se) = se_pipeline.take() {
                        info!("Stopped SE pipeline");
                    }

                    is_se_playing = false;

                    // 再生状態を初期化に戻す
                    playback_state = PlaybackState::WaitingForFirstSync;
                    info!("Audio system paused, waiting for system to be re-enabled");
                } else {
                    // システムが再有効化された場合
                    info!("✅ My system is re-enabled - resuming audio system");
                    playback_state = PlaybackState::WaitingForFirstSync;

                    // 有効化SEを再生するフラグを立てる
                    should_play_activation_se = true;
                }
            } else {
                // 他人向けのイベントは無視
                debug!(
                    my_addr = ?*my_addr_guard,
                    target_addr = %state.target_device_id,
                    "Ignoring SystemEnabledState for another device"
                );
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

        // 最新サーバー時間をtime_offsetから計算
        let current_offset = *time_offset.lock().unwrap();
        if current_offset != 0 { // オフセットが初期値(0)でなければ同期済みとみなす
            let client_now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64;
            let estimated_server_time_ns = (client_now_ns + current_offset) as u64;
            last_server_time_ns = Some(estimated_server_time_ns);
        }

        // システム有効化時のSE再生処理
        if should_play_activation_se && !is_se_playing {
            info!("🎵 システム有効化SE再生開始");
            should_play_activation_se = false;

            // SE再生中フラグを立てる
            is_se_playing = true;

            // 既存のSEパイプラインがあれば停止
            if let Some(old_se) = se_pipeline.take() {
                info!("🛑 既存のSEパイプラインを停止してクリーンアップ");
                if old_se.set_state(gst::State::Null).is_ok() {
                    wait_for_state(&old_se, gst::State::Null, Duration::from_millis(500), "se_cleanup_on_activation");
                }
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

            // BGMの音量を下げる（SE再生中）
            if let Some(ref act) = active {
                set_volume(&act.volume, 0.3);
                info!("🔉 BGM音量を0.3に下げました（SE再生開始）");
            }

            // 既存のSEパイプラインがあれば停止
            if let Some(old_se) = se_pipeline.take() {
                info!("🛑 既存のSEパイプラインを停止してクリーンアップ");
                if old_se.set_state(gst::State::Null).is_ok() {
                    wait_for_state(&old_se, gst::State::Null, Duration::from_millis(500), "se_cleanup_on_new_request");
                }
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
                        if se_pipe.set_state(gst::State::Null).is_ok() {
                            wait_for_state(&se_pipe, gst::State::Null, Duration::from_millis(500), "se_cleanup_on_eos");
                        }
                    }
                    // SE再生中フラグをリセット
                    is_se_playing = false;

                    // BGMの音量を元に戻す
                    if let Some(ref act) = active {
                        set_volume(&act.volume, 1.0);
                        info!("🔊 BGM音量を1.0に復元しました（SE再生完了）");
                    }
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



                    active = Some(act);



                    playback_start_time = Instant::now();
                    initial_server_time_ns = server_time_ns;
                    playback_state = PlaybackState::Playing;
                } else if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    // 同期なしフォールバック
                    let act = build_pipeline(&current_sound)?;
                    let _ = act.pipeline.set_state(gst::State::Playing);
                    set_volume(&act.volume, 1.0);



                    active = Some(act);





                    playback_start_time = Instant::now();
                    initial_server_time_ns = 0;
                    playback_state = PlaybackState::Playing;
                }
            }
            PlaybackState::Playing => {
                // 現在の再生位置をログ出力
                if let Some(ref act) = active {
                    if let Some(pos) = act.pipeline.query_position::<gst::ClockTime>() {
                        debug!("現在の再生位置: {} ns", pos.nseconds());
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

                    // 現在再生中のBGMと再生位置を表示
                    print!("🎵 現在のBGM: {}", current_sound);
                    if let Some(ref act) = active {
                        if let (Some(pos), Some(dur)) = (act.pipeline.query_position::<gst::ClockTime>(), act.pipeline.query_duration::<gst::ClockTime>()) {
                            let dur_ms = dur.mseconds();
                            if dur_ms > 0 {
                                let pos_ms = pos.mseconds();
                                let percent = (pos_ms as f64 / dur_ms as f64) * 100.0;
                                print!(" ({:.1}%)", percent);
                            } else {
                                print!(" (再生位置: {}ms)", pos.mseconds());
                            }
                        } else if let Some(pos) = act.pipeline.query_position::<gst::ClockTime>() {
                             print!(" (再生位置: {}ms)", pos.mseconds());
                        }
                    }
                    println!();

                    // 検知されているLocationとそのRSSIを表示（環境変数に関係なく常に表示）
                    let sound_map_guard = sound_map.lock().unwrap();
                    let mut all_locations_info: Vec<(String, String, Option<i16>)> = Vec::new();

                    for (addr, sound_file) in sound_map_guard.iter() {
                        if let Some(device) = detected_devices.get(addr) {
                            all_locations_info.push((addr.clone(), sound_file.clone(), Some(device.rssi)));
                        } else {
                            all_locations_info.push((addr.clone(), sound_file.clone(), None));
                        }
                    }

                    all_locations_info.sort_by(|a, b| {
                        // RSSIがある場合は降順、ない場合は後ろに
                        match (a.2, b.2) {
                            (Some(rssi_a), Some(rssi_b)) => rssi_b.cmp(&rssi_a),
                            (Some(_), None) => std::cmp::Ordering::Less,
                            (None, Some(_)) => std::cmp::Ordering::Greater,
                            (None, None) => a.0.cmp(&b.0), // RSSIがない場合はアドレスでソート
                        }
                    });

                    println!("📍 ロケーションの状態:");

                    if !all_locations_info.is_empty() {
                        for (addr, sound, rssi_opt) in all_locations_info {
                            match rssi_opt {
                                Some(rssi) => println!("  └─ Location: {} | Sound: {} | RSSI: {} dBm", addr, sound, rssi),
                                None => println!("  └─ Location: {} | Sound: {} | RSSI: (検知なし)", addr, sound),
                            }
                        }
                    } else {
                        println!("  (登録されたロケーションは現在ありません)");
                    }
                }

                // ドリフト補正（アクティブ側のみ）
                if let (Some(server_time_ns), Some(ref act)) = (last_server_time_ns, active.as_ref()) {
                    // 切替中と直後のウィンドウはシークを行わない
                    let in_switch_guard = switching || last_switch_end.map_or(false, |t| Instant::now().duration_since(t) < SWITCH_GUARD_WINDOW);
                    if initial_server_time_ns != 0 && !in_switch_guard && server_time_ns >= initial_server_time_ns {
                        let server_elapsed = (server_time_ns - initial_server_time_ns) as i64;
                        let client_elapsed = playback_start_time.elapsed().as_nanos() as i64;
                        let diff_real_ns = server_elapsed - client_elapsed;
                                                let diff_abs_s = (diff_real_ns.abs() as f64) / 1e9;
                                                debug!(diff_ns = diff_real_ns, "Drift detected");
                                                let new_rate: f64 = if diff_abs_s > 3.0 {
                                                    warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected (>3s), seeking active.");
                                                    let _ = seek_to_server_time(&act.pipeline, &act.bus, server_time_ns);
                                                    1.0
                                                } else {
                                                    let diff_s = diff_real_ns as f64 / 1e9;
                                                    const CORRECTION_TIME_S: f64 = 2.0;
                                                    (1.0 + diff_s / CORRECTION_TIME_S).clamp(0.9, 1.1)
                                                };
                                                debug!(rate = new_rate, "Calculated new playback rate");
                                                if let Some(ref p) = act.pitch { p.set_property("tempo", new_rate as f32); }
                        playback_start_time = Instant::now();
                        initial_server_time_ns = server_time_ns;
                    }
                }

                let desired_sound = {
                    let sound_map_guard = sound_map.lock().unwrap();

                    // 1. 現在の場所の情報を取得
                    let (current_device_addr, current_device_rssi) = {
                        let addr = sound_map_guard.iter()
                            .find(|(_, sound_file)| **sound_file == current_sound)
                            .map(|(addr, _)| addr.clone());
                        if let Some(addr) = addr {
                            let rssi = detected_devices.get(&addr).map_or(i16::MIN, |d| d.rssi);
                            (Some(addr), rssi)
                        } else {
                            (None, i16::MIN)
                        }
                    };

                    // 2. 最も強い「他の」場所を探す
                    let best_alternative_location = detected_devices.values()
                        .filter(|d| Some(d.address.as_str()) != current_device_addr.as_deref())
                        .filter(|d| sound_map_guard.contains_key(&d.address))
                        .max_by_key(|d| d.rssi);

                    // 3. 状態に応じてBGMを決定
                    if current_sound == default_sound {
                        // ケース1: 現在がデフォルトBGMの場合
                        if let Some(best_dev) = detected_devices.values()
                            .filter(|d| sound_map_guard.contains_key(&d.address))
                            .max_by_key(|d| d.rssi)
                        {
                            sound_map_guard.get(&best_dev.address).unwrap().clone() // 最も強い場所に切り替え
                        } else {
                            default_sound.clone() // 維持
                        }
                    } else {
                        // ケース2: 特定の場所のBGMを再生中の場合
                        if current_device_rssi >= MAINTAIN_THRESHOLD_RSSI {
                            // サブケース2a: 現在の場所を検知できており、RSSIも十分
                            if let Some(best_alternative) = best_alternative_location {
                                if best_alternative.rssi > current_device_rssi + 3 {
                                    sound_map_guard.get(&best_alternative.address).unwrap().clone() // 切り替え
                                } else {
                                    current_sound.clone() // 維持
                                }
                            } else {
                                current_sound.clone() // 維持
                            }
                        } else {
                            // サブケース2b: 現在の場所のRSSIが弱い、またはロストした
                            if let Some(best_alternative) = best_alternative_location {
                                // 他に切り替えるべき場所が存在するので、そちらに切り替える
                                sound_map_guard.get(&best_alternative.address).unwrap().clone()
                            } else {
                                // 代替がない場合のフォールバック処理
                                if current_device_rssi == i16::MIN {
                                    // ケース2b-1: 現在の場所が完全にロストした -> 維持
                                    info!(
                                        "Current location ('{}') lost and no alternatives found. Maintaining current sound.",
                                        current_sound
                                    );
                                    current_sound.clone()
                                } else {
                                    // ケース2b-2: RSSIが閾値以下になった -> デフォルトに戻す
                                    info!(
                                        current_sound = %current_sound,
                                        current_rssi = current_device_rssi,
                                        "Current location RSSI is below threshold and no alternatives found. Falling back to default.",
                                    );
                                    default_sound.clone()
                                }
                            }
                        }
                    }
                };

                // --- ログ出力 ---
                if last_log_time.elapsed() >= LOG_INTERVAL {
                    let sound_map_guard = sound_map.lock().unwrap();
                    let target_addr = sound_map_guard.iter()
                        .find(|(_, sound)| **sound == desired_sound)
                        .map(|(addr, _)| addr.clone());

                    let rssi = if let Some(ref addr) = target_addr {
                        detected_devices.get(addr).map(|d| d.rssi)
                    } else {
                        None
                    };

                    // 時刻取得 (人間可読形式)
                    let timestamp_str = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

                    // CSV書き込み: timestamp, bgm, address, rssi
                    if let Ok(mut file) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open("rssi_log.csv")
                    {
                        let addr_str = target_addr.unwrap_or_else(|| "none".to_string());
                        let rssi_str = rssi.map(|r| r.to_string()).unwrap_or_else(|| "".to_string());
                        if let Err(e) = writeln!(file, "{},{},{},{}", timestamp_str, desired_sound, addr_str, rssi_str) {
                             error!("Failed to write to log file: {}", e);
                        }
                    } else {
                         error!("Failed to open log file rssi_log.csv");
                    }
                    last_log_time = Instant::now();
                }
                // ----------------

                // 共有されている現在のロケーションアドレスを更新
                {
                    let mut current_addr = current_location_address.lock().unwrap();
                    if desired_sound == default_sound {
                        if current_addr.is_some() {
                            info!("Updating current location address to None (default)");
                            *current_addr = None;
                        }
                    } else {
                        let sound_map_guard = sound_map.lock().unwrap();
                        let new_addr = sound_map_guard.iter()
                            .find(|(_, sound_file)| **sound_file == desired_sound)
                            .map(|(addr, _)| addr.clone());

                        if *current_addr != new_addr {
                            info!(?new_addr, "Updating current location address");
                            *current_addr = new_addr;
                        }
                    }
                }


                // 非同期切り替えの完了チェック
                if let Ok(new_pipeline) = switch_rx.try_recv() {
                    info!("✅ Instant switch: Applying new pipeline.");

                    // 1. 古いパイプラインを即座に停止
                    if let Some(old_pipeline) = active.take() {
                        info!("Stopping old pipeline immediately.");
                        if let Err(e) = old_pipeline.pipeline.set_state(gst::State::Null) {
                            warn!("Failed to set old pipeline to NULL: {}", e);
                        }
                    }

                    // 2. 新しいパイプラインを即座に再生
                    info!("Starting new pipeline immediately.");
                    // 音量を最大に設定
                    set_volume(&new_pipeline.volume, 1.0);
                    // 再生開始
                    let _ = new_pipeline.pipeline.set_state(gst::State::Playing);

                    // 新しいパイプラインをアクティブに設定
                    active = Some(new_pipeline);



                    // 同期を再設定


                    playback_start_time = Instant::now();
                    if let Some(t) = last_server_time_ns {
                        initial_server_time_ns = t;
                    }

                    switching = false;
                    last_switch_end = Some(Instant::now());
                    info!("🎉 Instant switch completed.");
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
                    if let Some(server_time) = last_server_time_ns {
                        let request = SwitchRequest {
                            desired_sound: desired_sound.clone(),
                            server_time_ns: server_time,
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

                                    info!("⏸️  Paused状態でサーバー時間 {} ns に基づいてシーク", request.server_time_ns);
                                    let _ = next.pipeline.set_state(gst::State::Paused);
                                    wait_for_state(&next.pipeline, gst::State::Paused, Duration::from_secs(3), "async_switch_pause");

                                    // seek_to_server_time を使って正しい位置にシーク
                                    if let Err(e) = seek_to_server_time(&next.pipeline, &next.bus, request.server_time_ns) {
                                        error!("Failed to seek to server time during switch: {}", e);
                                    }
                                    info!("✓ シーク完了");


                                    // 🔥 重要：Paused状態のままメインスレッドに送信
                                    // メインスレッドで古いパイプラインを停止してからPlayingに切り替える
                                    info!("⏸️  パイプラインをPaused状態で準備完了、メインスレッドに送信");

                                    // 完成したパイプラインをメインスレッドに送信（Paused状態のまま）
                                    if let Err(e) = switch_tx_clone.blocking_send(next) {
                                        error!("Failed to send new pipeline: {}", e);
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to build pipeline: {}", e);
                                }
                            }
                        });
                    } else {
                        warn!("Cannot switch sound: server time is not available.");
                        switching = false; // switchingフラグをリセット
                    }
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
