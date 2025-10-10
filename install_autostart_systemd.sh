#!/bin/bash
# このスクリプトは systemd を使って setup_and_run.sh を初回起動時に自動実行します
# より現代的でクリーンな方法です

set -e

echo "========================================="
echo "Tsukimi Speaker systemd自動起動設定"
echo "========================================="

# ユーザー名を自動検出
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

echo "検出されたユーザー: ${CURRENT_USER}"

# スクリプトのパスを設定
SETUP_SCRIPT="/home/${CURRENT_USER}/tsukimi-speaker-rust/setup_and_run.sh"

# setup_and_run.sh の存在確認
if [ ! -f "$SETUP_SCRIPT" ]; then
    echo "エラー: ${SETUP_SCRIPT} が見つかりません"
    exit 1
fi

# setup_and_run.sh に実行権限を付与
echo "Step 1: setup_and_run.sh に実行権限を付与..."
sudo chmod +x "$SETUP_SCRIPT"
echo "✓ 実行権限を付与しました"

# systemd サービスファイルを作成
echo "Step 2: systemd サービスファイルを作成..."
sudo tee /etc/systemd/system/tsukimi-setup.service > /dev/null << EOF
[Unit]
Description=Tsukimi Speaker Initial Setup and Auto Start
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=$SETUP_SCRIPT
StandardOutput=journal
StandardError=journal
User=root
Environment="SUDO_USER=${CURRENT_USER}"

[Install]
WantedBy=multi-user.target
EOF

echo "✓ systemd サービスファイルを作成しました"

# サービスを有効化
echo "Step 3: サービスを有効化..."
sudo systemctl daemon-reload
sudo systemctl enable tsukimi-setup.service
echo "✓ サービスを有効化しました"

echo ""
echo "========================================="
echo "✓ 設定完了！"
echo "========================================="
echo ""
echo "次回起動時から自動的に以下が実行されます："
echo "  1. 初回起動時: セットアップ → 自動再起動"
echo "  2. 再起動後: プログラムが自動起動"
echo ""
echo "サービスの状態確認："
echo "  sudo systemctl status tsukimi-setup.service"
echo ""
echo "今すぐテストする場合は、以下を実行してください："
echo "  sudo systemctl start tsukimi-setup.service"
echo ""
