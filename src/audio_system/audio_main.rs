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

struct PipelineState {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    pitch: Option<gst::Element>,
    filesrc: gst::Element,
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
    let filesrc = pipeline.by_name("src").ok_or_else(|| anyhow!("filesrc not found"))?;
    let volume = pipeline.by_name("vol").ok_or_else(|| anyhow!("volume not found"))?;
    let pitch = pipeline.by_name("pch");
    Ok(PipelineState { pipeline, bus, pitch, filesrc, volume })
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

fn wait_for_buffering(bus: &gst::Bus, timeout: Duration, label: &str) -> bool {
    let start = Instant::now();
    let mut buffering_complete = false;

    while Instant::now().duration_since(start) < timeout {
        while let Some(msg) = bus.timed_pop(gst::ClockTime::from_nseconds(50_000_000)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Buffering(buffering_msg) => {
                    let percent = buffering_msg.percent();
                    debug!(?percent, label, "Buffering progress");
                    if percent >= 100 {
                        buffering_complete = true;
                        info!(label, "Buffering complete");
                        return true;
                    }
                }
                MessageView::Error(err) => {
                    error!(error=%err.error(), label, "Error during buffering");
                    return false;
                }
                _ => {}
            }
        }
        if buffering_complete {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // タイムアウトしてもバッファリングメッセージが来ない場合は続行
    warn!(label, "Buffering check timeout, continuing anyway");
    true
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
        max_volume_rssi: 0.0,  // 未使用（音量制御には使わない）
        min_volume_rssi: 0.0,  // 未使用（音量制御には使わない）
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

    'main_loop: loop {
        // バス処理（アクティブ優先、スタンバイも確認）
        if let Some(ref act) = active {
            while let Some(msg) = act.bus.timed_pop(gst::ClockTime::from_mseconds(5)) {
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
                        let new_rate: f64 = if diff_abs_s > 1.0 {
                            warn!(diff_s = diff_real_ns as f64 / 1e9, "Large drift detected, seeking active.");
                            let _ = seek_to_server_time(&act.pipeline, &act.bus, server_time_ns);
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

                let desired_sound = if let Some(device) = best_device {
                    let sound_map = sound_map.lock().unwrap();
                    sound_map.get(&device.address).cloned().unwrap_or_else(|| default_sound.clone())
                } else { default_sound.clone() };

                if desired_sound != current_sound {
                    info!(from=%current_sound, to=%desired_sound, "Parallel switch start");
                    switching = true;

                    // スタンバイパイプラインがあれば停止して破棄
                    // (filesrcのlocationプロパティは動的に変更できないため、毎回新規構築)
                    if let Some(old_standby) = standby.take() {
                        let _ = old_standby.pipeline.set_state(gst::State::Null);
                    }

                    // 新しいパイプラインを構築
                    let next = build_pipeline(&desired_sound)?;

                    // Paused → シーク → volume=0 → Playing
                    let _ = next.pipeline.set_state(gst::State::Paused);
                    wait_for_state(&next.pipeline, gst::State::Paused, Duration::from_secs(10), "standby_pause");
                    if let Some(server_time_ns) = last_server_time_ns { let _ = seek_to_server_time(&next.pipeline, &next.bus, server_time_ns); }
                    if let Some(ref p) = next.pitch { p.set_property("tempo", 1.0f32); }
                    set_volume(&next.volume, 0.0);
                    let _ = next.pipeline.set_state(gst::State::Playing);

                    // デコードウォームアップ: 再生直後のパーサ再同期やプリロールが落ち着くまで少し待つ
                    std::thread::sleep(Duration::from_millis(300));
                    wait_for_buffering(&next.bus, Duration::from_secs(5), "warmup_buffering");

                    // クロスフェード（短時間）
                    if let Some(ref act) = active {
                        let steps = 12;     // ステップ数（増やすほど滑らか）
                        let step_ms = 20;   // ステップ間隔（合計 ~240ms）
                        for i in 0..=steps {
                            let t = (i as f64) / (steps as f64);
                            let theta = t * std::f64::consts::FRAC_PI_2; // 0 -> π/2
                            let a = theta.cos(); // 現行の振幅係数
                            let b = theta.sin(); // 次の振幅係数
                            set_volume(&act.volume, a);
                            set_volume(&next.volume, b);
                            std::thread::sleep(Duration::from_millis(step_ms));
                        }
                    }

                    // 旧パイプラインを停止
                    if let Some(old) = active.take() {
                         let _ = old.pipeline.set_state(gst::State::Null);
                     }

                    // 切替確定
                    current_sound = desired_sound;
                    active = Some(next);
                    standby = None; // メモリ節約: 必要時に再構築
                    playback_start_time = Instant::now();
                    if let Some(t) = last_server_time_ns { initial_server_time_ns = t; }
                    switching = false;
                    last_switch_end = Some(Instant::now());
                    info!("Parallel switch completed");
                }
            }
        }
    }

    // 終了処理
    if let Some(act) = active { let _ = act.pipeline.set_state(gst::State::Null); }
    if let Some(st) = standby { let _ = st.pipeline.set_state(gst::State::Null); }
    Ok(())
}
