mod audio_system;
mod bluetooth_system;

use crate::audio_system::audio_main::audio_main;
use crate::bluetooth_system::bluetooth_main::bluetooth_scanner;
use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    // Bluetoothスキャナをバックグラウンドタスクとして実行
    let bluetooth_handle = tokio::spawn(async {
        if let Err(e) = bluetooth_scanner().await {
            eprintln!("Bluetooth scanner error: {}", e);
        }
    });

    // 同期的なaudio_main関数をspawn_blockingで実行
    let audio_handle = tokio::task::spawn_blocking(move || audio_main());

    // オーディオ再生タスクの結果を待つ
    match audio_handle.await {
        Ok(Ok(_)) => println!("Audio playback finished successfully."),
        Ok(Err(e)) => eprintln!("Audio playback error: {e}"),
        Err(e) => eprintln!("Audio task panicked: {e}"),
    }

    // Bluetoothタスクはバックグラウンドで実行し続ける
    // もしアプリ終了時にスキャンも止めたい場合は、ここでhandleをabortするなどの処理を追加
    bluetooth_handle.abort();

    Ok(())
}