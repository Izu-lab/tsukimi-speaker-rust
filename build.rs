use std::env;
use tonic_prost_build::configure;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 環境変数 `SKIP_PROTOC` が設定されている場合は、何もしないで終了
    if env::var("SKIP_PROTOC").is_ok() {
        println!("cargo:warning=Skipping protoc compilation because SKIP_PROTOC is set.");
        return Ok(());
    }

    configure()
        .out_dir("src/proto")
        .compile_protos(
            &["../TSUKIMKORO-2025/TSUKIMI_Backend/proto/device.proto", "../TSUKIMKORO-2025/TSUKIMI_Backend/proto/time.proto"],
            &["../TSUKIMKORO-2025/TSUKIMI_Backend/proto"],
        )?;
    Ok(())
}