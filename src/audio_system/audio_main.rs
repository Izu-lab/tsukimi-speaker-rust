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

enum PlaybackState { WaitingForFirstSync, Playing }

struct ActivePipeline {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    volume: gst::Element,
}

fn sink_name() -> &'static str {
    #[cfg(target_os = "linux")] { "pulsesink" }
    #[cfg(not(target_os = "linux"))] { "autoaudiosink" }
}

fn build_simple_pipeline(sound_path: &str) -> Result<ActivePipeline> {
    let sink = sink_name();
    let desc = format!(
        concat!(
            "filesrc location={} ! decodebin ! audioconvert ! ",
            "audio/x-raw,format=S16LE,rate=44100,channels=2 ! ",
            "volume name=vol ! ",
            "{} buffer-time=200000 latency-time=20000"
        ),
        sound_path,
        sink
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("Downcast pipeline failed"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("bus missing"))?;
    let volume = pipeline.by_name("vol").ok_or_else(|| anyhow!("volume missing"))?;
    Ok(ActivePipeline { pipeline, bus, volume })
}

fn wait_for_state(pipeline: &gst::Pipeline, target: gst::State, timeout: Duration, label: &str) -> bool {
    let start = Instant::now();
    loop {
        if Instant::now().duration_since(start) > timeout {
            warn!(?target, label, "Timeout waiting state");
            return false;
        }
        let (ret, current, pending) = pipeline.state(gst::ClockTime::from_mseconds(0));
        if let (Ok(_), c, gst::State::VoidPending) = (ret, current, pending) {
            if c == target { return true; }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn seek_pipeline_to_time(pipeline: &gst::Pipeline, bus: &gst::Bus, server_time_ns: u64) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(3);
    loop {
        if let Some(dur) = pipeline.query_duration::<gst::ClockTime>() {
            if dur.nseconds() > 0 {
                let t_ns = server_time_ns % dur.nseconds();
                let t = gst::ClockTime::from_nseconds(t_ns);
                pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t)?;
                let _ = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]);
                return Ok(());
            }
        }
        if Instant::now().duration_since(start) > timeout { return Ok(()); }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn set_vol(e: &gst::Element, v: f64) {
    e.set_property("volume", v);
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
    tracing_subscriber::fmt::try_init().ok();
    info!("Audio system (simple pipeline) started");
    gst::init()?;

    let mut playback_state = PlaybackState::WaitingForFirstSync;
    let default_sound = "tsukimi-main.mp3".to_string();
    let mut current_sound = default_sound.clone();
    let mut detected_devices: HashMap<String, Arc<DeviceInfo>> = HashMap::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

    let mut active: Option<ActivePipeline> = None;
    let mut initial_server_time_ns: u64 = 0;
    let mut last_server_time_ns: Option<u64> = None;
    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    'main: loop {
        // アクティブパイプラインのバスメンテ
        if let Some(ref ap) = active {
            while let Some(msg) = ap.bus.timed_pop(gst::ClockTime::from_mseconds(5)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(e) => {
                        error!(err=%e.error(), debug=?e.debug(), "Pipeline error");
                        break 'main;
                    }
                    MessageView::Eos(_) => {
                        debug!("EOS received, restarting playback");
                        // ループ再生のため、最初に戻る
                        if let Err(e) = ap.pipeline.seek_simple(
                            gst::SeekFlags::FLUSH,
                            gst::ClockTime::from_nseconds(0)
                        ) {
                            warn!(?e, "Failed to seek to beginning");
                        }
                    }
                    _ => {}
                }
            }
        }

        // サーバー時間取り込み
        while let Ok(t) = time_sync_rx.try_recv() {
            last_server_time_ns = Some(t);
        }

        match playback_state {
            PlaybackState::WaitingForFirstSync => {
                if let Some(t) = last_server_time_ns {
                    info!("Starting initial playback");
                    let ap = build_simple_pipeline(&current_sound)?;
                    ap.pipeline.set_state(gst::State::Paused)?;
                    wait_for_state(&ap.pipeline, gst::State::Paused, Duration::from_secs(5), "initial_paused");
                    seek_pipeline_to_time(&ap.pipeline, &ap.bus, t)?;
                    set_vol(&ap.volume, 1.0);
                    ap.pipeline.set_state(gst::State::Playing)?;
                    active = Some(ap);
                    initial_server_time_ns = t;
                    playback_state = PlaybackState::Playing;
                    continue;
                }
                if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    info!("Fallback: starting without sync");
                    let ap = build_simple_pipeline(&current_sound)?;
                    set_vol(&ap.volume, 1.0);
                    ap.pipeline.set_state(gst::State::Playing)?;
                    active = Some(ap);
                    playback_state = PlaybackState::Playing;
                    continue;
                }
            }
            PlaybackState::Playing => {
                // 設定更新
                if let Ok(new_setting) = sound_setting_rx.try_recv() {
                    info!(?new_setting, "Sound setting updated");
                }

                // デバイス更新
                while let Ok(di) = rx.try_recv() {
                    detected_devices.insert(di.address.clone(), di);
                }

                // 古いデバイスクリーンアップ
                if Instant::now().duration_since(last_cleanup) > CLEANUP_INTERVAL {
                    let n0 = detected_devices.len();
                    detected_devices.retain(|_, d| Instant::now().duration_since(d.last_seen) < CLEANUP_INTERVAL);
                    if n0 != detected_devices.len() {
                        debug!("Cleaned devices");
                    }
                    last_cleanup = Instant::now();
                }

                // ドリフト補正
                if let (Some(t), Some(ref ap)) = (last_server_time_ns, active.as_ref()) {
                    if initial_server_time_ns != 0 {
                        let server_elapsed = (t - initial_server_time_ns) as i64;
                        let client_pos = ap.pipeline.query_position::<gst::ClockTime>()
                            .map(|c| c.nseconds())
                            .unwrap_or(0) as i64;
                        let diff = server_elapsed - client_pos;
                        if (diff.abs() as f64 / 1e9) > 1.0 {
                            debug!(diff_sec = diff as f64 / 1e9, "Correcting drift");
                            let _ = seek_pipeline_to_time(&ap.pipeline, &ap.bus, t);
                            initial_server_time_ns = t;
                        }
                    }
                }

                // 望ましいサウンドの決定
                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    detected_devices.values()
                        .filter_map(|d| sound_map.get(&d.address).cloned())
                        .next()
                };
                let desired = best_device.unwrap_or_else(|| default_sound.clone());

                if desired != current_sound {
                    info!(from=%current_sound, to=%desired, "Switching sound with fade");

                    // 新しいパイプラインを準備
                    let new_ap = build_simple_pipeline(&desired)?;
                    new_ap.pipeline.set_state(gst::State::Paused)?;
                    wait_for_state(&new_ap.pipeline, gst::State::Paused, Duration::from_secs(5), "new_paused");

                    if let Some(t) = last_server_time_ns {
                        seek_pipeline_to_time(&new_ap.pipeline, &new_ap.bus, t)?;
                        initial_server_time_ns = t;
                    }

                    set_vol(&new_ap.volume, 0.0);
                    new_ap.pipeline.set_state(gst::State::Playing)?;
                    std::thread::sleep(Duration::from_millis(100));

                    // クロスフェード
                    if let Some(ref old_ap) = active {
                        let steps = 20;
                        let step_ms = 25;
                        for i in 0..=steps {
                            let t = (i as f64) / (steps as f64);
                            let theta = t * std::f64::consts::FRAC_PI_2;
                            let old_vol = theta.cos();
                            let new_vol = theta.sin();
                            set_vol(&old_ap.volume, old_vol);
                            set_vol(&new_ap.volume, new_vol);
                            std::thread::sleep(Duration::from_millis(step_ms));
                        }
                    }

                    // 古いパイプラインを停止
                    if let Some(old_ap) = active.take() {
                        let _ = old_ap.pipeline.set_state(gst::State::Null);
                        wait_for_state(&old_ap.pipeline, gst::State::Null, Duration::from_secs(2), "old_null");
                    }

                    active = Some(new_ap);
                    current_sound = desired;
                    info!("Fade completed");
                }
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    // 終了処理
    info!("Shutting down audio system");
    if let Some(ap) = active {
        let _ = ap.pipeline.set_state(gst::State::Null);
        wait_for_state(&ap.pipeline, gst::State::Null, Duration::from_secs(2), "shutdown_null");
    }
    info!("Audio system shut down complete");
    Ok(())
}
