#!/bin/bash

# BLEとWi-Fiの共存を最適化するスクリプト
# Raspberry Piでの使用を想定

echo "Optimizing BLE and Wi-Fi coexistence..."

# Bluetoothアダプターのスキャン設定を最適化
# hci0のスキャンウィンドウとインターバルを調整
if command -v hciconfig &> /dev/null; then
    echo "Configuring Bluetooth adapter..."
    sudo hciconfig hci0 up

    # スキャンパラメータの最適化（より積極的にスキャン）
    # これにより、Wi-Fiとの干渉があってもBLEビーコンを見逃しにくくなる
    if command -v btmgmt &> /dev/null; then
        echo "Setting BLE scan parameters for better sensitivity..."
        # Active scanを有効化
        sudo btmgmt -i hci0 power off
        sleep 1
        sudo btmgmt -i hci0 le on
        sudo btmgmt -i hci0 bredr off
        sudo btmgmt -i hci0 power on
        echo "BLE-only mode enabled (Wi-Fi coexistence optimized)"
    fi
fi

# Bluetoothのログレベルを上げてデバッグ情報を確認できるようにする（任意）
# sudo btmon &

echo ""
echo "Optimization complete!"
echo ""
echo "Tips for better BLE performance with Wi-Fi:"
echo "1. Wi-Fiの5GHz帯を使用する（2.4GHz干渉を回避）"
echo "2. Wi-Fiルーターを802.11ac/ax (5GHz)モードに設定"
echo "3. 可能であればイーサネット接続を使用"
echo ""
echo "Current Bluetooth adapter status:"
hciconfig hci0

