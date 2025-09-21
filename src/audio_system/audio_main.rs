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
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, instrument, warn};

#[instrument(skip(rx, time_sync_rx))]
pub fn audio_main(
    mut rx: broadcast::Receiver<Arc<DeviceInfo>>,
    mut time_sync_rx: mpsc::Receiver<String>,
) -> Result<()> {
    info!("Audio system main loop started.");
    // ## 1. GStreamerと共有バッファの初期化 ##
    if let Err(e) = gst::init() {
        error!("Failed to initialize GStreamer: {}", e);
        return Err(anyhow!("Failed to initialize GStreamer"));
    }
    info!("GStreamer initialized successfully.");

    // --- ここから追加・変更 ---
    // TODO: ご自身の環境に合わせて、Bluetoothアドレスとサウンドファイル名を変更してください。
    let mut sound_map = HashMap::new();
    // 例: "sound.mp3"を再生させたいデバイスのアドレスをここに追加
    sound_map.insert(
        "00:11:22:33:44:55".to_string(),
        "sound.mp3".to_string(),
    );
    // 例: 別のサウンドファイルを再生させたい場合。ファイルは別途用意する必要があります。
    // sound_map.insert(
    //     "AA:BB:CC:DD:EE:FF".to_string(),
    //     "sound2.mp3".to_string(),
    // );

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
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("No output device available"))?;

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
            if let Ok(time_str) = time_sync_rx.try_recv() {
                if let Ok(time_ns) = time_str.parse::<u64>() {
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
                } else {
                    warn!("Failed to parse time string: {}", time_str);
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
            let best_device = detected_devices
                .values()
                .filter(|d| sound_map.contains_key(&d.address))
                .max_by_key(|d| d.rssi);

            if let Some(device) = best_device {
                if let Some(new_sound) = sound_map.get(&device.address) {
                    if current_sound.as_deref() != Some(new_sound.as_str()) {
                        info!(
                            new_sound,
                            device_address = %device.address,
                            "Switching sound"
                        );
                        pipeline.set_state(gst::State::Ready)?;
                        filesrc.set_property("location", new_sound);
                        pipeline.set_state(gst::State::Playing)?;
                        current_sound = Some(new_sound.clone());
                    }
                    update_volume_from_rssi(device, &volume);
                }
            } else {
                // sound_mapに登録されたデバイスが1つも見つからない場合
                if current_sound.is_some() {
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
    // RSSIを音量レベル(0.0-1.0)にマッピング
    let rssi = device_info.rssi as f64;
    const MAX_RSSI: f64 = -40.0; // 音量1.0になるRSSI値
    const MIN_RSSI: f64 = -90.0; // 音量0.0になるRSSI値

    let mut volume_level = (rssi - MIN_RSSI) / (MAX_RSSI - MIN_RSSI);
    volume_level = volume_level.clamp(0.0, 1.0); // 0.0-1.0の範囲に収める

    debug!(volume = volume_level, "Update volume");

    // GStreamerのvolumeエレメントに音量を設定
    volume_element.set_property("volume", volume_level);
}
