#!/bin/bash
# BLE スキャン性能チェックスクリプト
# ラズベリーパイで実行してBluetooth環境を診断します

echo "=== Bluetooth環境診断 ==="
echo ""

echo "1. Bluetoothアダプタの確認:"
hciconfig -a
echo ""

echo "2. Bluetoothサービスの状態:"
systemctl status bluetooth | grep -E "(Active|Loaded)"
echo ""

echo "3. 現在のスキャンパラメータ:"
if [ -f /sys/kernel/debug/bluetooth/hci0/conn_min_interval ]; then
    echo "  Connection min interval: $(cat /sys/kernel/debug/bluetooth/hci0/conn_min_interval 2>/dev/null || echo 'N/A')"
    echo "  Connection max interval: $(cat /sys/kernel/debug/bluetooth/hci0/conn_max_interval 2>/dev/null || echo 'N/A')"
fi
echo ""

echo "4. 10秒間のBLEスキャンテスト (hcitool使用):"
echo "   対象デバイスが広告を送信していることを確認してください..."
timeout 10 hcitool lescan 2>&1 | head -20
echo ""

echo "5. 推奨される最適化設定:"
echo "   以下のコマンドを実行すると、スキャン感度が向上する可能性があります:"
echo ""
echo "   sudo hciconfig hci0 reset"
echo "   sudo hciconfig hci0 up"
echo "   sudo btmgmt power off"
echo "   sudo btmgmt power on"
echo "   sudo btmgmt le on"
echo ""

echo "6. BlueZ設定の確認:"
if [ -f /etc/bluetooth/main.conf ]; then
    echo "   /etc/bluetooth/main.conf の関連設定:"
    grep -E "^(Privacy|FastConnectable|DiscoverableTimeout)" /etc/bluetooth/main.conf 2>/dev/null || echo "   デフォルト設定を使用中"
else
    echo "   /etc/bluetooth/main.conf が見つかりません"
fi
echo ""

echo "7. カーネルログからBluetooth関連のエラーをチェック:"
dmesg | grep -i bluetooth | tail -10
echo ""

echo "=== 診断完了 ==="
echo ""
echo "性能が低い場合の対策:"
echo "  - Bluetoothアダプタを物理的に近づける"
echo "  - USB3.0ポートからUSB2.0ポートに変更（干渉を減らす）"
echo "  - 2.4GHz WiFiとの干渉を確認（可能なら5GHz WiFiを使用）"
echo "  - 電源供給が十分か確認（ラズパイの電源不足はBT性能に影響）"

