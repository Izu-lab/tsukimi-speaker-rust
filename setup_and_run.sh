#!/bin/bash
# Raspberry Pi初回セットアップとプログラム自動起動スクリプト

# ユーザー名を自動検出（piまたはtsukimi）
# systemd サービスから実行される場合は SUDO_USER または実際のユーザーを検出
if [ -n "$SUDO_USER" ]; then
    CURRENT_USER="$SUDO_USER"
elif [ -n "$USER" ] && [ "$USER" != "root" ]; then
    CURRENT_USER="$USER"
else
    # /home ディレクトリから実際のユーザーを検出
    if [ -d "/home/pi" ]; then
        CURRENT_USER="pi"
    elif [ -d "/home/tsukimi" ]; then
        CURRENT_USER="tsukimi"
    else
        CURRENT_USER=$(ls /home | head -n 1)
    fi
fi

USER_HOME="/home/${CURRENT_USER}"

INSTALL_FLAG="${USER_HOME}/.tsukimi_setup_complete"
LOG_FILE="${USER_HOME}/tsukimi_setup.log"
PROJECT_DIR="${USER_HOME}/tsukimi-speaker-rust"

# ログディレクトリとファイルを確実に作成
if ! mkdir -p "$(dirname "$LOG_FILE")" 2>/dev/null; then
    # ホームディレクトリに作成できない場合は /tmp を使用
    LOG_FILE="/tmp/tsukimi_setup.log"
fi

# ログファイルを作成（存在しない場合）
if ! touch "$LOG_FILE" 2>/dev/null; then
    LOG_FILE="/tmp/tsukimi_setup_$(date +%s).log"
    touch "$LOG_FILE"
fi

# ログファイルの権限を設定（root で実行されている場合）
if [ "$(whoami)" = "root" ] && [ "$CURRENT_USER" != "root" ]; then
    chown "$CURRENT_USER:$CURRENT_USER" "$LOG_FILE" 2>/dev/null || true
fi

# エラーが発生したら即座に終了
set -e

# ログ関数
log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $1" | tee -a "$LOG_FILE"
}

log "========================================="
log "Tsukimi Speaker セットアップスクリプト開始"
log "User: ${CURRENT_USER}"
log "Home: ${USER_HOME}"
log "Project: ${PROJECT_DIR}"
log "========================================="

# セットアップが完了しているかチェック
if [ -f "$INSTALL_FLAG" ]; then
    log "セットアップ済み。プログラムを起動します..."

    # プログラムのディレクトリに移動
    if [ ! -d "$PROJECT_DIR" ]; then
        log "エラー: プロジェクトディレクトリが見つかりません: ${PROJECT_DIR}"
        exit 1
    fi

    cd "$PROJECT_DIR"

    # プログラムを実行
    log "Tsukimi Speaker プログラム起動中..."
    ./target/release/tsukimi-speaker 2>&1 | tee -a "$LOG_FILE"

    exit 0
fi

log "初回セットアップを開始します..."

# 1. システムアップデートとパッケージインストール
log "Step 1: システムアップデートとパッケージインストール"
sudo apt-get update 2>&1 | tee -a "$LOG_FILE"
sudo apt-get upgrade -y 2>&1 | tee -a "$LOG_FILE"

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
    pi-bluetooth 2>&1 | tee -a "$LOG_FILE"

# 2. Rustのインストール
log "Step 2: Rustのインストール"
if ! command -v rustc &> /dev/null; then
    log "Rustをインストール中..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y 2>&1 | tee -a "$LOG_FILE"
    source $HOME/.cargo/env
    log "Rust インストール完了"
else
    log "Rust は既にインストールされています"
fi

# 3. プロジェクトの確認
log "Step 3: プロジェクトの準備"
if [ ! -d "$PROJECT_DIR" ]; then
    log "エラー: プロジェクトディレクトリが見つかりません: ${PROJECT_DIR}"
    log "手動でプロジェクトファイルを ${PROJECT_DIR} に配置してください。"
    exit 1
else
    log "プロジェクトディレクトリ確認: ${PROJECT_DIR}"
fi

cd "$PROJECT_DIR"

# 4. プログラムのビルド
log "Step 4: プログラムのビルド"
source $HOME/.cargo/env
cargo build --release 2>&1 | tee -a "$LOG_FILE"
log "ビルド完了"

# 5. Bluetooth設定
log "Step 5: Bluetooth設定"
sudo systemctl enable bluetooth 2>&1 | tee -a "$LOG_FILE"
sudo systemctl start bluetooth 2>&1 | tee -a "$LOG_FILE"

# 6. PulseAudio設定
log "Step 6: PulseAudio設定"
# PulseAudioの自動起動設定
mkdir -p "${USER_HOME}/.config/systemd/user/"
cat > "${USER_HOME}/.config/systemd/user/pulseaudio.service" << 'EOF'
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

systemctl --user enable pulseaudio 2>&1 | tee -a "$LOG_FILE"

# 7. 自動起動設定（systemdサービス）
log "Step 7: 自動起動設定"
sudo tee /etc/systemd/system/tsukimi-speaker.service > /dev/null << EOF
[Unit]
Description=Tsukimi Speaker Service
After=network.target bluetooth.target pulseaudio.service
Wants=bluetooth.target

[Service]
Type=simple
User=${CURRENT_USER}
WorkingDirectory=${PROJECT_DIR}
ExecStart=${PROJECT_DIR}/target/release/tsukimi-speaker
Restart=always
RestartSec=10
Environment="RUST_LOG=info"
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

sudo systemctl daemon-reload 2>&1 | tee -a "$LOG_FILE"
sudo systemctl enable tsukimi-speaker.service 2>&1 | tee -a "$LOG_FILE"

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
