#!/bin/bash

# スクリプトが失敗した場合に備えて、コマンドの実行を即座に停止する
set -e

echo "--- Raspberry Piの依存ライブラリセットアップを開始します ---"

# パッケージリストの更新
echo "--- パッケージリストを更新しています... ---"
sudo apt-get update

# プログラムのビルドと実行に必要なシステムライブラリをインストール
echo "--- 必要なライブラリとツールをインストールしています... ---"
sudo apt-get install -y \
    build-essential \
    pkg-config \
    libssl-dev \
    protobuf-compiler \
    libasound2-dev \
    libbluetooth-dev \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-alsa \
    pulseaudio

echo ""
echo "--- 依存ライブラリのインストールが完了しました！ ---"
echo ""
echo "--- オーディオシステムの状態を確認します ---"
# PulseAudioがアクティブでない場合、起動を試みる
if ! pactl info &>/dev/null; then
    echo "PulseAudioが起動していないようです。起動を試みます..."
    pulseaudio --start
    echo "PulseAudioを起動しました。"
else
    echo "PulseAudioは既に起動しています。"
fi

echo ""
echo "利用可能なALSA出力デバイス:"
aplay -l
echo ""
echo "セットアップ完了後、'cargo build'でプロジェクトをビルドしてください。"