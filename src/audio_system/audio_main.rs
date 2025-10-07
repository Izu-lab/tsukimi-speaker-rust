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

struct MixerState {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    vol_a: gst::Element,
    vol_b: gst::Element,
}

struct DecoderState {
    pipeline: gst::Pipeline,
    bus: gst::Bus,
    pitch: Option<gst::Element>,
}

fn sink_name() -> &'static str {
    #[cfg(target_os = "linux")] { "pulsesink" }
    #[cfg(not(target_os = "linux"))] { "autoaudiosink" }
}

fn build_mixer_pipeline() -> Result<MixerState> {
    let sink = sink_name();
    let desc = format!(
        concat!(
            "interaudiosrc channel=a blocksize=8192 ! ",
            "audio/x-raw,format=F32LE,rate=44100,channels=2 ! ",
            "volume name=volA ! queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! mix. ",
            "interaudiosrc channel=b blocksize=8192 ! ",
            "audio/x-raw,format=F32LE,rate=44100,channels=2 ! ",
            "volume name=volB ! queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! mix. ",
            "audiomixer name=mix start-time-selection=first ! ",
            "audio/x-raw,format=S16LE,rate=44100,channels=2 ! ",
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 ! ",
            "{} buffer-time=200000 latency-time=20000"
        ),
        sink
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("Downcast mixer pipeline failed"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("mixer bus missing"))?;
    let vol_a = pipeline.by_name("volA").ok_or_else(|| anyhow!("volA missing"))?;
    let vol_b = pipeline.by_name("volB").ok_or_else(|| anyhow!("volB missing"))?;

    Ok(MixerState { pipeline, bus, vol_a, vol_b })
}

fn build_decoder_pipeline(sound_path: &str, channel: &str) -> Result<DecoderState> {
    // CPU負荷削減：pitchは無効化し、シンプルなパイプラインに
    let desc = format!(
        concat!(
            "filesrc location={} blocksize=16384 ! decodebin ! audioconvert ! ",
            "audio/x-raw,format=F32LE,rate=44100,channels=2 ! ",
            "queue max-size-buffers=0 max-size-bytes=0 max-size-time=100000000 ! ",
            "interaudiosink channel={} blocksize=16384"
        ),
        sound_path,
        channel,
    );
    let pipeline = gst::parse::launch(&desc)?.downcast::<gst::Pipeline>().map_err(|_| anyhow!("Downcast decoder pipeline failed"))?;
    let bus = pipeline.bus().ok_or_else(|| anyhow!("decoder bus missing"))?;
    // pitchは使用しない（CPU負荷削減）
    Ok(DecoderState { pipeline, bus, pitch: None })
}

fn wait_for_state(pipeline: &gst::Pipeline, target: gst::State, timeout: Duration, label: &str) -> bool {
    let start = Instant::now();
    loop {
        if Instant::now().duration_since(start) > timeout { warn!(?target, label, "Timeout waiting state"); return false; }
        let (ret, current, pending) = pipeline.state(gst::ClockTime::from_mseconds(0));
        if let (Ok(_), c, gst::State::VoidPending) = (ret, current, pending) { if c == target { return true; } }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn seek_pipeline_to_time(pipeline: &gst::Pipeline, bus: &gst::Bus, server_time_ns: u64) -> Result<()> {
    // duration待ち（最大3秒）
    let start = Instant::now();
    let timeout = Duration::from_secs(3);
    loop {
        if let Some(dur) = pipeline.query_duration::<gst::ClockTime>() { if dur.nseconds() > 0 {
            let t_ns = server_time_ns % dur.nseconds();
            let t = gst::ClockTime::from_nseconds(t_ns);
            pipeline.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, t)?;
            let _ = bus.timed_pop_filtered(Some(gst::ClockTime::from_seconds(5)), &[gst::MessageType::AsyncDone]);
            return Ok(());
        }}
        if Instant::now().duration_since(start) > timeout { return Ok(()); }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_until_flowing(p: &gst::Pipeline, max_wait: Duration) -> bool {
    let start = Instant::now();
    let mut last = p.query_position::<gst::ClockTime>().map(|c| c.nseconds()).unwrap_or(0);
    loop { if Instant::now().duration_since(start) > max_wait { return false; }
        if let Some(pos) = p.query_position::<gst::ClockTime>() { let ns = pos.nseconds(); if ns > last { return true; } last = ns; }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn set_vol(e: &gst::Element, v: f64) { e.set_property("volume", v); }

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
    info!("Audio system (audiomixer) started");
    gst::init()?;

    // 状態
    let mut playback_state = PlaybackState::WaitingForFirstSync;
    let default_sound = "tsukimi-main.mp3".to_string();
    let mut current_sound = default_sound.clone();
    let mut detected_devices: HashMap<String, Arc<DeviceInfo>> = HashMap::new();
    let mut last_cleanup = Instant::now();
    const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

    // ミキサーパイプライン
    let mixer = build_mixer_pipeline()?;
    mixer.pipeline.set_state(gst::State::Playing)?;
    wait_for_state(&mixer.pipeline, gst::State::Playing, Duration::from_secs(3), "mixer_play");
    set_vol(&mixer.vol_a, 1.0);
    set_vol(&mixer.vol_b, 0.0);

    // デコーダ（アクティブ/スタンバイ）
    let mut active: Option<DecoderState> = None; // channel "a" か "b"
    let mut standby: Option<DecoderState> = None;
    let mut active_channel = "a".to_string();

    // 同期関連
    let mut initial_server_time_ns: u64 = 0;
    let mut last_server_time_ns: Option<u64> = None;
    let sync_wait_start = Instant::now();
    const SYNC_TIMEOUT: Duration = Duration::from_secs(5);

    'main: loop {
        // ミキサーバスのメンテ
        while let Some(msg) = mixer.bus.timed_pop(gst::ClockTime::from_mseconds(5)) {
            use gst::MessageView; match msg.view() { MessageView::Error(e) => {
                error!(err=%e.error(), debug=?e.debug(), "Mixer error"); break 'main;
            }, _=>{} }
        }
        // サーバー時間取り込み（最新だけ）
        while let Ok(t) = time_sync_rx.try_recv() { last_server_time_ns = Some(t); }

        match playback_state {
            PlaybackState::WaitingForFirstSync => {
                if let Some(t) = last_server_time_ns {
                    // 初回のアクティブデコーダを起動（channel=a）
                    let dec = build_decoder_pipeline(&current_sound, "a")?;
                    dec.pipeline.set_state(gst::State::Paused)?;
                    wait_for_state(&dec.pipeline, gst::State::Paused, Duration::from_secs(5), "dec_a_paused");
                    seek_pipeline_to_time(&dec.pipeline, &dec.bus, t)?;
                    if let Some(ref pch) = dec.pitch { pch.set_property("tempo", 1.0f32); }
                    dec.pipeline.set_state(gst::State::Playing)?;
                    wait_until_flowing(&dec.pipeline, Duration::from_millis(500));
                    active = Some(dec);
                    active_channel = "a".into();
                    initial_server_time_ns = t;
                    playback_state = PlaybackState::Playing;
                    continue;
                }
                if Instant::now().duration_since(sync_wait_start) > SYNC_TIMEOUT {
                    // フォールバック: 同期なしで開始
                    let dec = build_decoder_pipeline(&current_sound, "a")?;
                    dec.pipeline.set_state(gst::State::Playing)?;
                    active = Some(dec); active_channel = "a".into();
                    playback_state = PlaybackState::Playing; continue;
                }
            }
            PlaybackState::Playing => {
                // 設定更新
                if let Ok(new_setting) = sound_setting_rx.try_recv() { info!(?new_setting, "Sound setting updated"); }
                // デバイス更新
                while let Ok(di) = rx.try_recv() { detected_devices.insert(di.address.clone(), di); }
                // 古いデバイスクリーンアップ
                if Instant::now().duration_since(last_cleanup) > CLEANUP_INTERVAL { let n0 = detected_devices.len();
                    detected_devices.retain(|_, d| Instant::now().duration_since(d.last_seen) < CLEANUP_INTERVAL);
                    if n0!=detected_devices.len(){ debug!("Cleaned devices"); } last_cleanup=Instant::now(); }

                // ドリフト補正（アクティブのみ、切替時は抑止）
                if let (Some(t), Some(ref dec)) = (last_server_time_ns, active.as_ref()) { let server_elapsed = if initial_server_time_ns>0 {(t-initial_server_time_ns) as i64} else {0};
                    // シンプル: 大きくずれていたらシーク
                    if initial_server_time_ns!=0 { let client_pos = dec.pipeline.query_position::<gst::ClockTime>().map(|c| c.nseconds()).unwrap_or(0) as i64;
                        let diff = (server_elapsed*1) - client_pos; // 同一単位(ns)の概算
                        if diff.abs() as f64 / 1e9 > 1.0 { seek_pipeline_to_time(&dec.pipeline, &dec.bus, t)?; initial_server_time_ns = t; }
                    }
                }

                // 望ましいサウンドの決定（ヒステリシスは省略、まず正しさ優先）
                let best_device = {
                    let sound_map = sound_map.lock().unwrap();
                    detected_devices.values().filter_map(|d| sound_map.get(&d.address).cloned()).next()
                };
                let desired = best_device.unwrap_or_else(|| default_sound.clone());

                if desired != current_sound {
                    info!(from=%current_sound, to=%desired, "Switch using audiomixer");
                    let standby_channel = if active_channel == "a" { "b" } else { "a" };

                    // フェード前: 新しいチャンネルのボリュームを0に設定
                    let (from_vol, to_vol) = if standby_channel == "b" {
                        (&mixer.vol_a, &mixer.vol_b)
                    } else {
                        (&mixer.vol_b, &mixer.vol_a)
                    };
                    set_vol(to_vol, 0.0);

                    // 既存スタンバイを廃棄
                    if let Some(st) = standby.take() { let _ = st.pipeline.set_state(gst::State::Null); }

                    // 新スタンバイ作成
                    let next_dec = build_decoder_pipeline(&desired, standby_channel)?;
                    next_dec.pipeline.set_state(gst::State::Paused)?;
                    wait_for_state(&next_dec.pipeline, gst::State::Paused, Duration::from_secs(5), "standby_paused");
                    if let Some(t) = last_server_time_ns {
                        seek_pipeline_to_time(&next_dec.pipeline, &next_dec.bus, t)?;
                        initial_server_time_ns = t;
                    }
                    if let Some(ref p) = next_dec.pitch { p.set_property("tempo", 1.0f32); }
                    next_dec.pipeline.set_state(gst::State::Playing)?;

                    // flowing 待ち（新しいストリームが実際に流れ始めるまで待機）
                    if !wait_until_flowing(&next_dec.pipeline, Duration::from_millis(1000)) {
                        warn!("New decoder not flowing in time, proceeding anyway");
                    }

                    // さらに少し待機して、バッファを安定させる
                    std::thread::sleep(Duration::from_millis(100));

                    // 等電力クロスフェード（volA/volB）
                    let steps = 20; // ステップ数を増やしてより滑らかに
                    let step_ms = 25;
                    for i in 0..=steps {
                        let t = (i as f64)/(steps as f64);
                        let theta = t * std::f64::consts::FRAC_PI_2;
                        let a = theta.cos();
                        let b = theta.sin();
                        set_vol(from_vol, a);
                        set_vol(to_vol, b);
                        std::thread::sleep(Duration::from_millis(step_ms));
                    }

                    // フェード完了後: 古いデコーダを停止
                    if let Some(old) = active.take() {
                        let _ = old.pipeline.set_state(gst::State::Null);
                    }

                    // 掛け替え
                    active = Some(next_dec);
                    current_sound = desired;
                    active_channel = standby_channel.into();

                    // 最終的なボリューム状態を確実に設定
                    if active_channel == "a" {
                        set_vol(&mixer.vol_a, 1.0);
                        set_vol(&mixer.vol_b, 0.0);
                    } else {
                        set_vol(&mixer.vol_a, 0.0);
                        set_vol(&mixer.vol_b, 1.0);
                    }

                    info!(channel=%active_channel, "Fade completed");
                }
            }
        }
    }

    // 終了処理
    info!("Shutting down audio system");
    if let Some(dec) = active {
        let _ = dec.pipeline.set_state(gst::State::Null);
        wait_for_state(&dec.pipeline, gst::State::Null, Duration::from_secs(2), "active_null");
    }
    if let Some(dec) = standby {
        let _ = dec.pipeline.set_state(gst::State::Null);
        wait_for_state(&dec.pipeline, gst::State::Null, Duration::from_secs(2), "standby_null");
    }
    let _ = mixer.pipeline.set_state(gst::State::Null);
    wait_for_state(&mixer.pipeline, gst::State::Null, Duration::from_secs(2), "mixer_null");
    info!("Audio system shut down complete");
    Ok(())
}
