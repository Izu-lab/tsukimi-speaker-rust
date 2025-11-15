#!/bin/bash
# PulseAudio接続問題を解決するため、システムサービスからユーザーサービスに変更

set -e

echo "========================================="
echo "Tsukimi Speaker 自動起動修正スクリプト"
echo "========================================="
echo ""

# ユーザー名を検出
CURRENT_USER=$(whoami)
if [ "$CURRENT_USER" = "root" ]; then
    echo "エラー: このスクリプトはroot以外のユーザーで実行してください"
    exit 1
fi

USER_HOME="/home/${CURRENT_USER}"
PROJECT_DIR="${USER_HOME}/tsukimi-speaker-rust"

echo "User: ${CURRENT_USER}"
echo "Project: ${PROJECT_DIR}"
echo ""

# 1. 既存のシステムサービスを停止・無効化
echo "Step 1: 既存のシステムサービスを停止・無効化..."
sudo systemctl stop tsukimi-speaker.service 2>/dev/null || true
sudo systemctl disable tsukimi-speaker.service 2>/dev/null || true
sudo systemctl stop tsukimi-setup.service 2>/dev/null || true
sudo systemctl disable tsukimi-setup.service 2>/dev/null || true
echo "✓ 既存サービスを停止しました"
echo ""

# 2. ユーザーサービスディレクトリを作成
echo "Step 2: ユーザーサービスディレクトリを作成..."
mkdir -p "${USER_HOME}/.config/systemd/user/"
echo "✓ ディレクトリを作成しました"
echo ""

# 3. PulseAudio ユーザーサービスを有効化
echo "Step 3: PulseAudio ユーザーサービスを有効化..."
systemctl --user --now enable pulseaudio.socket 2>/dev/null || true
systemctl --user --now enable pulseaudio.service 2>/dev/null || true
echo "✓ PulseAudio を有効化しました"
echo ""

# 4. ユーザーサービスファイルを作成
echo "Step 4: ユーザーサービスファイルを作成..."
cat > "${USER_HOME}/.config/systemd/user/tsukimi-speaker.service" << EOF
[Unit]
Description=Tsukimi Speaker Service
After=default.target pulseaudio.service sound.target
Requires=pulseaudio.service
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=${PROJECT_DIR}
ExecStartPre=/bin/sleep 5
ExecStart=${PROJECT_DIR}/target/aarch64-unknown-linux-gnu/debug/tsukimi-speaker
Restart=always
RestartSec=15
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
EOF
echo "✓ ユーザーサービスファイルを作成しました"
echo ""

# 5. ユーザーサービスを有効化
echo "Step 5: ユーザーサービスを有効化..."
systemctl --user daemon-reload
systemctl --user enable tsukimi-speaker.service
echo "✓ ユーザーサービスを有効化しました"
echo ""

# 6. loginctlでlinger を有効化（ログアウト後もサービスを実行し続ける）
echo "Step 6: ログアウト後も実行し続けるよう設定..."
sudo loginctl enable-linger ${CURRENT_USER}
echo "✓ linger を有効化しました"
echo ""

# 7. ユーザーサービスを起動
echo "Step 7: ユーザーサービスを起動..."
systemctl --user start tsukimi-speaker.service
echo "✓ サービスを起動しました"
echo ""

echo "========================================="
echo "✓ 修正完了！"
echo "========================================="
echo ""
echo "サービスの状態確認："
echo "  systemctl --user status tsukimi-speaker.service"
echo ""
echo "ログの確認："
echo "  journalctl --user -u tsukimi-speaker.service -f"
echo ""
echo "サービスの停止："
echo "  systemctl --user stop tsukimi-speaker.service"
echo ""
echo "サービスの再起動："
echo "  systemctl --user restart tsukimi-speaker.service"
echo ""
echo "サービスの無効化（自動起動を止める）："
echo "  systemctl --user disable tsukimi-speaker.service"
echo ""

