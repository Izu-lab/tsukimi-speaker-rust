#!/bin/bash
# このスクリプトは、デバッグビルドを行い、デバッグログを有効にしてアプリケーションを実行します。

# デバッグモードでビルド
cargo build

# デバッグログを有効にして実行
RUST_LOG=debug ./target/debug/tsukimi-speaker
