use tonic_prost_build::configure;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure()
        .out_dir("src/proto")
        .compile_protos(
            &["../TSUKIMKORO-2025/TSUKIMI_Backend/proto/device.proto", "../TSUKIMKORO-2025/TSUKIMI_Backend/proto/time.proto"],
            &["../TSUKIMKORO-2025/TSUKIMI_Backend/proto"],
        )?;
    Ok(())
}