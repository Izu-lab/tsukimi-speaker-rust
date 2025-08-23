use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use glib::object::Cast;
use gstreamer as gst;
use gstreamer::prelude::{ElementExt, GstBinExt};
use gstreamer_app as gst_app;
use ringbuf::HeapRb;

pub fn audio_main() -> Result<()> {
    // ## 1. GStreamerと共有バッファの初期化 ##
    gst::init().expect("gstの初期化に失敗");

    // オーディオフォーマットを定義 (CPALとGStreamerで合わせる)
    const CHANNELS: u32 = 2;
    const SAMPLE_RATE: u32 = 44100;

    // GStreamerがデコードしたデータを入れる共有バッファを作成
    // サイズはSAMPLE_RATE * CHANNELSで1秒分くらいを確保
    let rb = HeapRb::<f32>::new((SAMPLE_RATE * CHANNELS) as usize);
    let (mut producer, mut consumer) = rb.split();

    // ## 2. GStreamerパイプラインの構築 ##
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

    // _streamは、Dropされると再生が止まるため、ここで束縛しておく必要がある
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

    Ok(())
}