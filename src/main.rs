use btleplug::api::{Central, Manager as _, Peripheral, ScanFilter};
use btleplug::platform::{Adapter, Manager, PeripheralId};
use futures::stream::StreamExt;
use std::time::Duration;
use glib::object::Cast;
use gstreamer::prelude::{ElementExt, GstBinExt};
use tokio::time;
use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use gstreamer as gst;
use gstreamer_app as gst_app;
use ringbuf::HeapRb;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ## 1. GStreamerと共有バッファの初期化 ##
    gst::init()?;

    // オーディオフォーマットを定義 (CPALとGStreamerで合わせる)
    const CHANNELS: u32 = 2;
    const SAMPLE_RATE: u32 = 44100;

    // GStreamerがデコードしたデータを入れる共有バッファを作成
    // サイズはSAMPLE_RATE * CHANNELSで1秒分くらいを確保
    let rb = HeapRb::<f32>::new((SAMPLE_RATE * CHANNELS) as usize);
    let (mut producer, mut consumer) = rb.split();

    // ## 2. GStreamerパイプラインの構築 ##
    // filesrc: ファイルを読む
    // decodebin: 適切なデコーダを自動選択
    // audioconvert: オーディオ形式を変換
    // audioresample: サンプルレートを変換
    // appsink: アプリケーションにデータを渡す
    let pipeline_str = format!(
        "filesrc location=sound.mp3 ! decodebin ! audioconvert ! audioresample ! appsink name=sink caps=audio/x-raw,format=F32LE,rate={},channels={}",
        SAMPLE_RATE, CHANNELS
    );

    let pipeline = gst::parse::launch(&pipeline_str)?;

    // ElementをPipelineにダウンキャストする
    let pipeline = pipeline
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("Element is not a pipeline"))?;

    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| anyhow!("Failed to get sink element"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow!("Sink element is not an appsink!"))?;

    // appsinkにコールバックを設定
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                // 新しいサンプルが来たら、バッファを取得して共有バッファ(producer)に書き込む
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;

                // GStreamerのデータ (u8スライス) をf32のスライスに変換
                let samples_f32: &[f32] = bytemuck::cast_slice(map.as_slice());

                // データを書き込む
                producer.push_slice(samples_f32);

                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    // ## 3. CPALオーディオストリームの構築 ##
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or_else(|| anyhow!("No output device available"))?;

    // CPALのコンフィグを設定
    let config = cpal::StreamConfig {
        channels: CHANNELS as u16,
        sample_rate: cpal::SampleRate(SAMPLE_RATE),
        buffer_size: cpal::BufferSize::Default,
    };

    // CPALのオーディオコールバック
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // スピーカーに書き込むデータを要求されたら、共有バッファ(consumer)から読み出す
            let written = consumer.pop_slice(data);
            // バッファが空でデータが足りない場合は、残りを無音で埋める
            data[written..].iter_mut().for_each(|s| *s = 0.0);
        },
        |err| eprintln!("An error occurred on the output audio stream: {}", err),
        None,
    )?;

    // ## 4. 再生の開始と終了処理 ##
    stream.play()?;
    pipeline.set_state(gst::State::Playing)?;

    println!("再生を開始しました。再生終了まで待機します...");

    // GStreamerのバスからメッセージを待つ
    let bus = pipeline.bus().unwrap();
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(_) => {
                println!("再生が終了しました。");
                break;
            }
            MessageView::Error(err) => {
                eprintln!("GStreamer Error: {}", err.error());
                eprintln!("Debug info: {:?}", err.debug());
                break;
            }
            _ => (),
        }
    }

    pipeline.set_state(gst::State::Null)?;

    // Bluetoothデバイス管理マネージャーを初期化
    let manager = Manager::new().await?;

    // 最初に見つかったアダプターを取得
    let adapters = manager.adapters().await?;
    let central = adapters
        .into_iter()
        .nth(0)
        .ok_or("Bluetooth adapter not found")?;

    // イベントのストリームを取得
    let mut events = central.events().await?;

    // スキャンを開始
    println!("Scanning for BLE devices...");
    central.start_scan(ScanFilter::default()).await?;

    // OSがスキャン結果を収集するのを少し待つ
    time::sleep(Duration::from_secs(2)).await;

    // イベントをループで待機
    while let Some(event) = events.next().await {
        // デバイスが発見または更新されたイベントを処理
        if let btleplug::api::CentralEvent::DeviceDiscovered(id) |
        btleplug::api::CentralEvent::DeviceUpdated(id) = event {
            on_event_receive(&central, &id).await;
        }
    }

    Ok(())
}

async fn on_event_receive(central: &Adapter, id: &PeripheralId) {
    // デバイス（ペリフェラル）の情報を取得
    if let Ok(p) = central.peripheral(&id).await {
        if let Ok(Some(props)) = p.properties().await {
            // RSSIが取得できた場合のみ表示
            if let Some(rssi) = props.rssi {
                println!(
                    "Device: {:?} ({:?}), RSSI: {} dBm",
                    props.local_name.unwrap_or_else(|| "Unknown".to_string()),
                    p.address(),
                    rssi
                );
            }
        }
    }
}