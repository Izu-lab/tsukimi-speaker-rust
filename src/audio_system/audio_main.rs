use crate::DeviceInfo;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use glib::object::Cast;
use gstreamer as gst;
use gstreamer::prelude::*;
use glib::object::ObjectExt;
use gstreamer_app as gst_app;use ringbuf::HeapRb;
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};

pub fn audio_main(
    mut rx: broadcast::Receiver<DeviceInfo>,
    mut time_sync_rx: mpsc::Receiver<String>,
) -> Result<()> {
    // ## 1. GStreamerと共有バッファの初期化 ##
    gst::init().expect("gstの初期化に失敗");

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
    let mut detected_devices = HashMap::<String, DeviceInfo>::new();
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
            let written = consumer.pop_slice(data);
            data[written..].iter_mut().for_each(|s| *s = 0.0);
        },
        |err| eprintln!("An error occurred on the output audio stream: {}", err),
        None,
    )?;

    _stream.play().expect("ストリームの開始に失敗");

    // ## 4. 再生の開始と終了処理 ##
    pipeline.set_state(gst::State::Playing)?;

    println!("再生を開始しました。再生終了まで待機します...");

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
                        println!("再生が終了しました。次のデバイスを待っています...");
                    }
                    MessageView::Error(err) => {
                        eprintln!("GStreamer Error: {}", err.error());
                        eprintln!("Debug info: {:?}", err.debug());
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

                            println!(
                                "Received seek time: {:?}, Duration: {:?}, Actual seek time: {:?}",
                                seek_time,
                                duration,
                                actual_seek_time
                            );

                            // 再生位置を更新
                            if let Err(e) = pipeline.seek_simple(
                                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                actual_seek_time,
                            ) {
                                eprintln!("Failed to seek: {}", e);
                            }
                        } // durationが0の場合はシークしない
                    } else {
                        // durationが取得できない場合は、とりあえずそのままシークしてみる
                        println!("Seeking to: {:?}", seek_time);
                        if let Err(e) = pipeline.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            seek_time,
                        ) {
                            eprintln!("Failed to seek: {}", e);
                        }
                    }
                } else {
                    eprintln!("Failed to parse time string: {}", time_str);
                }
            }

            // --- Bluetoothデバイス情報の受信と更新 ---
            while let Ok(device_info) = rx.try_recv() {
                detected_devices.insert(device_info.address.clone(), device_info);
            }

            // --- 古いデバイス情報のクリーンアップ ---
            let now = std::time::Instant::now();
            if now.duration_since(last_cleanup) > CLEANUP_INTERVAL {
                detected_devices.retain(|_, device_info| now.duration_since(device_info.last_seen) < CLEANUP_INTERVAL);
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
                        println!("Switching sound to {} for device {}", new_sound, device.address);
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
                    println!("No mapped devices found, stopping playback.");
                    pipeline.set_state(gst::State::Ready)?;
                    current_sound = None;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(100));
        }

    pipeline.set_state(gst::State::Null)?;

    Ok(())
}

/// RSSI値に基づいてGStreamerの音量を更新する
fn update_volume_from_rssi(device_info: &DeviceInfo, volume_element: &gst::Element) {
    // RSSIを音量レベル(0.0-1.0)にマッピング
    let rssi = device_info.rssi as f64;
    const MAX_RSSI: f64 = -40.0; // 音量1.0になるRSSI値
    const MIN_RSSI: f64 = -90.0; // 音量0.0になるRSSI値

    let mut volume_level = (rssi - MIN_RSSI) / (MAX_RSSI - MIN_RSSI);
    volume_level = volume_level.clamp(0.0, 1.0); // 0.0-1.0の範囲に収める

    println!(
        "Address: {}, RSSI: {} -> Volume: {:.2}",
        device_info.address,
        device_info.rssi,
        volume_level
    );

    // GStreamerのvolumeエレメントに音量を設定
    volume_element.set_property("volume", volume_level);
}
