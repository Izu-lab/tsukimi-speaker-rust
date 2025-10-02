use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use glib::object::Cast;
use glib::object::ObjectExt;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use ringbuf::HeapRb;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn};

#[instrument(skip(rx, time_sync_rx, sound_map))]
pub fn audio_main(
    mut rx: broadcast::Receiver<Arc<DeviceInfo>>,
    mut time_sync_rx: mpsc::Receiver<u64>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) -> Result<()> {
    info!("Audio system main loop started.");
    // ## 1. GStreamerと共有バッファの初期化 ##
    if let Err(e) = gst::init() {
        error!("Failed to initialize GStreamer: {}", e);
        return Err(anyhow!("Failed to initialize GStreamer"));
    }
    info!("GStreamer initialized successfully.");

    // フェードの状態を管理するenum
    enum FadeState {
        None,
        FadingOut {
            start_time: std::time::Instant,
            target_sound: String,
        },
        FadingIn {
            start_time: std::time::Instant,
            target_volume: f64,
        },
    }
    let mut fade_state = FadeState::None;
    const FADE_DURATION: std::time::Duration = std::time::Duration::from_millis(500); // 0.5秒でフェード

    // --- ここから追加・変更 ---
    // 現在再生中のサウンドファイル名を保持
    let mut current_sound = None::<String>;
    let mut detected_devices = HashMap::<String, Arc<DeviceInfo>>::new();
    let mut last_cleanup = std::time::Instant::now();
    const CLEANUP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
    // --- ここまで追加・変更 ---

    // オーディオフォーマットを定義 (CPALとGStreamerで合わせる)
    const CHANNELS: u32 = 2;
    const SAMPLE_RATE: u32 = 44100;

    // GStreamerがデコードしたデータを入れる共有バッファを作成
    let rb = HeapRb::<f32>::new((SAMPLE_RATE * CHANNELS) as usize);
    let (mut producer, mut consumer) = rb.split();

    // ## 2. GStreamerパイプラインの構築 ##
    let pipeline_str = format!(
        "filesrc name=src location=sound.mp3 ! decodebin ! volume name=vol ! audioconvert ! audioresample ! appsink name=sink caps=audio/x-raw,format=F32LE,rate={},channels={}",
        SAMPLE_RATE, CHANNELS
    );

    let pipeline = gst::parse::launch(&pipeline_str)?;

    // ElementをPipelineにダウンキャストする
    let pipeline = pipeline
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Element is not a pipeline"))?;

    // パイプラインからエレメントを取得
    let filesrc = pipeline
        .by_name("src")
        .ok_or_else(|| anyhow!("Failed to get src element"))?;
    let volume = pipeline
        .by_name("vol")
        .ok_or_else(|| anyhow!("Failed to get volume element"))?;
    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("Failed to get sink element"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("Sink element is not an appsink!"))?;

    // appsinkにコールバックを設定
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let _span = tracing::debug_span!("gst_new_sample").entered();
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                let samples_f32: &[f32] = bytemuck::cast_slice(map.as_slice());
                producer.push_slice(samples_f32);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // ## 3. CPALオーディオストリームの構築 ##
    let host = cpal::default_host();

    info!("Searching for audio device 'plughw:CARD=MAX98357A'...");
    let device = host.output_devices()?
        .find(|d| {
            if let Ok(name) = d.name() {
                name.starts_with("plughw:CARD=MAX98357A")
            } else {
                false
            }
        })
        .ok_or_else(|| anyhow!("Failed to find 'plughw:CARD=MAX98357A'. Please check `aplay -L` and device connection."))?;

    info!("Selected audio device: {}", device.name()?);

    let config = cpal::StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    let _stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            let _span = tracing::trace_span!("cpal_output_stream").entered();
            let written = consumer.pop_slice(data);
            data[written..].iter_mut().for_each(|s| *s = 0.0);
        },
        |err| error!("An error occurred on the output audio stream: {}", err),
        None,
    )?;
    info!("CPAL output stream built successfully.");

    if let Err(e) = _stream.play() {
        error!("Failed to start CPAL stream: {}", e);
        return Err(anyhow!("Failed to start CPAL stream"));
    }
    info!("CPAL stream started successfully.");


    // ## 4. 再生の開始と終了処理 ##
    pipeline.set_state(gst::State::Playing)?;

    info!("GStreamer pipeline state set to Playing. Waiting for playback to finish...");

    let bus = pipeline.bus().unwrap();
    // ## 5. メインループ ##
        'main_loop: loop {
            // --- GStreamerメッセージ処理 ---
            while let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(10)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(_) => {
                        pipeline.set_state(gst::State::Ready)?;
                        current_sound = None;
                        info!("Playback finished (EOS). Waiting for next device...");
                    }
                    MessageView::Error(err) => {
                        error!(
                            "GStreamer pipeline error: {}, Debug info: {:?}",
                            err.error(),
                            err.debug()
                        );
                        break 'main_loop;
                    }
                    _ => (),
                }
            }

            // --- 時間同期情報の受信 ---
            if let Ok(time_ns) = time_sync_rx.try_recv() {
                let seek_time = gst::ClockTime::from_nseconds(time_ns);

                // パイプラインから音源の長さを取得
                if let Some(duration) = pipeline.query_duration::<gst::ClockTime>() {
                    if duration.nseconds() > 0 {
                        // シーク時間を音源の長さで割った余りを計算してループさせる
                        let actual_seek_ns = seek_time.nseconds() % duration.nseconds();
                        let actual_seek_time = gst::ClockTime::from_nseconds(actual_seek_ns);

                        debug!(
                            received_seek_time = ?seek_time,
                            duration = ?duration,
                            actual_seek_time = ?actual_seek_time,
                            "Seeking with loop"
                        );

                        // 再生位置を更新
                        if let Err(e) = pipeline.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            actual_seek_time,
                        ) {
                            warn!("Failed to seek in pipeline: {}", e);
                        }
                    } // durationが0の場合はシークしない
                } else {
                    // durationが取得できない場合は、とりあえずそのままシークしてみる
                    debug!(seek_time = ?seek_time, "Seeking without duration");
                    if let Err(e) = pipeline.seek_simple(
                        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                        seek_time,
                    ) {
                        warn!("Failed to seek (no duration): {}", e);
                    }
                }
            }

            // --- Bluetoothデバイス情報の受信と更新 ---
            while let Ok(device_info) = rx.try_recv() {
                debug!(device = ?device_info, "Received device info");
                detected_devices.insert(device_info.address.clone(), device_info);
            }

            // --- 古いデバイス情報のクリーンアップ ---
            let now = std::time::Instant::now();
            if now.duration_since(last_cleanup) > CLEANUP_INTERVAL {
                let initial_count = detected_devices.len();
                detected_devices.retain(|_, device_info| now.duration_since(device_info.last_seen) < CLEANUP_INTERVAL);
                let final_count = detected_devices.len();
                if initial_count != final_count {
                    debug!(
                        cleaned_count = initial_count - final_count,
                        remaining_count = final_count,
                        "Cleaned up old devices."
                    );
                }
                last_cleanup = now;
            }

            // --- 再生するサウンドの決定 ---
            let best_device = {
                let sound_map = sound_map.lock().unwrap();
                let my_address_guard = my_address.lock().unwrap();
                let my_addr_opt = my_address_guard.as_deref();
                let points = *current_points.lock().unwrap();

                let mut candidates: Vec<_> = detected_devices
                    .values()
                    .filter(|d| sound_map.contains_key(&d.address))
                    .collect();

                // ポイントとRSSIでソート
                // 1. ポイントが高い順 (自分自身のデバイスであれば現在のポイント、そうでなければ0)
                // 2. RSSIが高い順
                candidates.sort_by(|a, b| {
                    let a_points = my_addr_opt.map_or(0, |my_addr| if a.address == my_addr { points } else { 0 });
                    let b_points = my_addr_opt.map_or(0, |my_addr| if b.address == my_addr { points } else { 0 });
                    b_points.cmp(&a_points).then_with(|| b.rssi.cmp(&a.rssi))
                });

                candidates.first().cloned()
            };

            // --- フェード処理 ---
            let mut next_fade_state = None;
            match &fade_state {
                FadeState::FadingOut { start_time, target_sound } => {
                    let elapsed = start_time.elapsed();
                    if elapsed >= FADE_DURATION {
                        // フェードアウト完了
                        volume.set_property("volume", 0.0);

                        // 音源切り替え
                        debug!("Switching sound to {}", target_sound);
                        pipeline.set_state(gst::State::Ready)?;
                        filesrc.set_property("location", target_sound.clone());
                        pipeline.seek_simple(gst::SeekFlags::FLUSH, gst::ClockTime::from_seconds(0))?;
                        pipeline.set_state(gst::State::Playing)?;
                        current_sound = Some(target_sound.clone());

                        // フェードインへ移行
                        let target_volume = if let Some(device) = &best_device {
                            get_volume_from_rssi(device)
                        } else {
                            1.0
                        };
                        next_fade_state = Some(FadeState::FadingIn {
                            start_time: std::time::Instant::now(),
                            target_volume,
                        });
                    } else {
                        // フェードアウト中
                        let progress = elapsed.as_secs_f64() / FADE_DURATION.as_secs_f64();
                        // cosカーブで 1.0 -> 0.0 (実際はsinを反転)
                        let eased_progress = (progress * std::f64::consts::FRAC_PI_2).sin();
                        let new_volume = 1.0 - eased_progress;
                        volume.set_property("volume", new_volume.clamp(0.0, 1.0));
                    }
                }
                FadeState::FadingIn { start_time, target_volume } => {
                    let elapsed = start_time.elapsed();
                    if elapsed >= FADE_DURATION {
                        // フェードイン完了
                        volume.set_property("volume", *target_volume);
                        next_fade_state = Some(FadeState::None);
                        debug!("Fade-in complete.");
                    } else {
                        // フェードイン中
                        let progress = elapsed.as_secs_f64() / FADE_DURATION.as_secs_f64();
                        // sinカーブで 0.0 -> target_volume
                        let new_volume = *target_volume * (progress * std::f64::consts::FRAC_PI_2).sin();
                        volume.set_property("volume", new_volume.clamp(0.0, 1.0));
                    }
                }
                FadeState::None => { // 何もしない
                }
            }

            if let Some(new_state) = next_fade_state {
                fade_state = new_state;
            }

            // --- 音源切り替えのトリガーと音量調整 ---
            if let Some(device) = best_device {
                let sound_map = sound_map.lock().unwrap();
                if let Some(new_sound) = sound_map.get(&device.address) {
                    if matches!(fade_state, FadeState::None) && current_sound.as_deref() != Some(new_sound.as_str()) {
                        info!(
                            new_sound,
                            device_address = %device.address,
                            "Switching sound"
                        );
                        // フェードアウトを開始
                        fade_state = FadeState::FadingOut {
                            start_time: std::time::Instant::now(),
                            target_sound: new_sound.clone(),
                        };
                    }
                    // RSSIによる音量調整はフェード中でないときだけ行う
                    if matches!(fade_state, FadeState::None) {
                        update_volume_from_rssi(&device, &volume);
                    }
                }
            } else {
                // sound_mapに登録されたデバイスが1つも見つからない場合
                if current_sound.is_some() && matches!(fade_state, FadeState::None) {
                    info!("No mapped devices found, stopping playback.");
                    pipeline.set_state(gst::State::Ready)?;
                    current_sound = None;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

    pipeline.set_state(gst::State::Null)?;

    info!("Audio system main loop finished.");
    Ok(())
}

/// RSSI値に基づいてGStreamerの音量を更新する
#[instrument(skip(volume_element), fields(device_address = %device_info.address, rssi = device_info.rssi))]
fn update_volume_from_rssi(device_info: &Arc<DeviceInfo>, volume_element: &gst::Element) {
    let volume_level = get_volume_from_rssi(device_info);
    debug!(volume = volume_level, "Update volume");
    volume_element.set_property("volume", volume_level);
}

/// RSSI値から音量レベル(0.0-1.0)を計算する
fn get_volume_from_rssi(device_info: &Arc<DeviceInfo>) -> f64 {
    let rssi = device_info.rssi as f64;
    const MAX_RSSI: f64 = -40.0; // 音量1.0になるRSSI値
    const MIN_RSSI: f64 = -90.0; // 音量0.0になるRSSI値

    let volume_level = (rssi - MIN_RSSI) / (MAX_RSSI - MIN_RSSI);
    volume_level.clamp(0.0, 1.0) // 0.0-1.0の範囲に収める
}
