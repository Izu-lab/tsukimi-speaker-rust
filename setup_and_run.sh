#!/bin/bash
# Raspberry Pi初回セットアップとプログラム自動起動スクリプト

set -e  # エラーが発生したら即座に終了

INSTALL_FLAG="/home/pi/.tsukimi_setup_complete"
LOG_FILE="/home/pi/tsukimi_setup.log"

# ログ関数
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1" | tee -a "$LOG_FILE"
}

log "========================================="
log "Tsukimi Speaker セットアップスクリプト開始"
log "========================================="

# セットアップが完了しているかチェック
if [ -f "$INSTALL_FLAG" ]; then
    log "セットアップ済み。プログラムを起動します..."

    # プログラムのディレクトリに移動
    cd /home/tsukimi/tsukimi-speaker-rust

    # プログラムを実行
    log "Tsukimi Speaker プログラム起動中..."
    ./target/aarch64-unknown-linux-gnu/debug/tsukimi-speaker 2>&1 | tee -a "$LOG_FILE"

    exit 0
fi

log "初回セットアップを開始します..."

# 1. システムアップデートとパッケージインストール
log "Step 1: システムアップデートとパッケージインストール"
sudo apt-get update | tee -a "$LOG_FILE"
sudo apt-get upgrade -y | tee -a "$LOG_FILE"

# 必要なパッケージをインストール
log "必要なパッケージをインストール中..."
sudo apt-get install -y \
    git \
    curl \
    build-essential \
    libdbus-1-dev \
    pkg-config \
    libgstreamer1.0-dev \
    libgstreamer-plugins-base1.0-dev \
    libglib2.0-dev \
    libasound2-dev \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-alsa \
    gstreamer1.0-pulseaudio \
    pulseaudio \
    pulseaudio-module-bluetooth \
    bluez \
    bluez-tools \
    bluetooth \
    pi-bluetooth | tee -a "$LOG_FILE"

# 2. Rustのインストール
log "Step 2: Rustのインストール"
if ! command -v rustc &> /dev/null; then
    log "Rustをインストール中..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y | tee -a "$LOG_FILE"
    source $HOME/.cargo/env
    log "Rust インストール完了"
else
    log "Rust は既にインストールされています"
fi

# 3. プロジェクトのクローン（存在しない場合）
log "Step 3: プロジェクトの準備"
if [ ! -d "/home/pi/tsukimi-speaker-rust" ]; then
    log "プロジェクトディレクトリが見つかりません。手動でデプロイしてください。"
    # ここではプロジェクトファイルが既に配置されていることを想定
else
    log "プロジェクトディレクトリ確認: /home/pi/tsukimi-speaker-rust"
fi

cd /home/pi/tsukimi-speaker-rust

# 4. プログラムのビルド
log "Step 4: プログラムのビルド"
source $HOME/.cargo/env
cargo build --release | tee -a "$LOG_FILE"
log "ビルド完了"

# 5. Bluetooth設定
log "Step 5: Bluetooth設定"
sudo systemctl enable bluetooth | tee -a "$LOG_FILE"
sudo systemctl start bluetooth | tee -a "$LOG_FILE"

# 6. PulseAudio設定
log "Step 6: PulseAudio設定"
# PulseAudioの自動起動設定
mkdir -p /home/pi/.config/systemd/user/
cat > /home/pi/.config/systemd/user/pulseaudio.service << 'EOF'
[Unit]
Description=PulseAudio Sound System
After=sound.target

[Service]
Type=notify
ExecStart=/usr/bin/pulseaudio --daemonize=no --log-target=journal
Restart=on-failure

[Install]
WantedBy=default.target
EOF

systemctl --user enable pulseaudio | tee -a "$LOG_FILE"

# 7. 自動起動設定（systemdサービス）
log "Step 7: 自動起動設定"
sudo tee /etc/systemd/system/tsukimi-speaker.service > /dev/null << EOF
[Unit]
Description=Tsukimi Speaker Service
After=network.target bluetooth.target pulseaudio.service
Wants=bluetooth.target

[Service]
Type=simple
User=pi
WorkingDirectory=/home/pi/tsukimi-speaker-rust
ExecStart=/home/pi/tsukimi-speaker-rust/target/release/tsukimi-speaker
Restart=always
RestartSec=10
Environment="RUST_LOG=info"
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload | tee -a "$LOG_FILE"
sudo systemctl enable tsukimi-speaker.service | tee -a "$LOG_FILE"

# 8. セットアップ完了フラグを作成
log "Step 8: セットアップ完了フラグを作成"
touch "$INSTALL_FLAG"
echo "Setup completed at $(date)" > "$INSTALL_FLAG"

log "========================================="
log "セットアップ完了！"
log "システムを再起動します..."
log "再起動後、プログラムが自動的に起動します"
log "========================================="

# 再起動
sleep 3
sudo reboot

