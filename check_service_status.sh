#!/bin/bash
# サービスの状態を詳しく確認するスクリプト

echo "========================================="
echo "Tsukimi Speaker サービス診断"
echo "========================================="
echo ""

echo "1. サービスの状態確認："
echo "---"
sudo systemctl status tsukimi-speaker.service --no-pager
echo ""

echo "2. サービスが有効化されているか："
echo "---"
sudo systemctl is-enabled tsukimi-speaker.service
echo ""

echo "3. サービスのログ（最新50行）："
echo "---"
sudo journalctl -u tsukimi-speaker.service -n 50 --no-pager
echo ""

echo "4. バイナリの存在確認："
echo "---"
CURRENT_USER=$(whoami)
if [ "$CURRENT_USER" = "root" ]; then
    CURRENT_USER=${SUDO_USER:-pi}
fi
BINARY_PATH="/home/${CURRENT_USER}/tsukimi-speaker-rust/target/aarch64-unknown-linux-gnu/debug/tsukimi-speaker"
if [ -f "$BINARY_PATH" ]; then
    echo "✓ バイナリ存在: $BINARY_PATH"
    ls -lh "$BINARY_PATH"
else
    echo "✗ バイナリが見つかりません: $BINARY_PATH"
fi
echo ""

echo "5. 作業ディレクトリの確認："
echo "---"
WORK_DIR="/home/${CURRENT_USER}/tsukimi-speaker-rust"
if [ -d "$WORK_DIR" ]; then
    echo "✓ 作業ディレクトリ存在: $WORK_DIR"
    ls -la "$WORK_DIR" | head -n 20
else
    echo "✗ 作業ディレクトリが見つかりません: $WORK_DIR"
fi
echo ""

echo "6. セットアップ完了フラグの確認："
echo "---"
FLAG_FILE="/home/${CURRENT_USER}/.tsukimi_setup_complete"
if [ -f "$FLAG_FILE" ]; then
    echo "✓ セットアップ完了フラグ存在"
    cat "$FLAG_FILE"
else
    echo "✗ セットアップ完了フラグが見つかりません"
fi
echo ""

echo "7. サービスファイルの内容："
echo "---"
cat /etc/systemd/system/tsukimi-speaker.service
echo ""

echo "8. Bluetooth と PulseAudio の状態："
echo "---"
echo "Bluetooth:"
sudo systemctl status bluetooth --no-pager | head -n 5
echo ""
echo "PulseAudio (user service):"
systemctl --user status pulseaudio --no-pager | head -n 5 || echo "PulseAudio user service not running"
echo ""

echo "========================================="
echo "診断完了"
echo "========================================="

