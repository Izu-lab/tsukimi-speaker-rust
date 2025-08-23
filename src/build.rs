fn main() -> Result<(), Box<dyn std::error::Error>> {
    // tonic_buildを使用して.protoファイルをコンパイルするように設定します。
    // .configure() を使うことで、さまざまなオプションを指定できます。
    tonic_build::configure()
        // ここにコンパイルしたい.protoファイルのパスを配列で指定します。
        // プロジェクトのルートからの相対パスで記述するのが一般的です。
        // 例: &["proto/service1.proto", "proto/service2.proto"]
        .compile(
            &["proto/helloworld/helloworld.proto"],
            // .protoファイル内で他の.protoファイルをimportしている場合、
            // その検索パス（インクルードパス）をここで指定します。
            // 通常は.protoファイルが置かれているディレクトリを指定します。
            &["proto/helloworld"],
        )?; // コンパイルに失敗した場合はエラーを返します。

    // ビルドスクリプトが成功したことを示します。
    Ok(())
}