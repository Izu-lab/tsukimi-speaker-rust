#!/bin/bash

# このスクリプトは、MAX98357A I2S DACを有効にするために、
# Raspberry Piのシステム設定を変更します。
# 実行後には再起動が必要です。

set -e

echo "--- MAX98357A I2S DACのセットアップを開始します ---"

# --- /boot/config.txt のバックアップと編集 ---
echo "--- /boot/config.txt を設定しています... ---"

# 念のためバックアップを作成
sudo cp /boot/config.txt /boot/config.txt.bak
echo "/boot/config.txt を /boot/config.txt.bak にバックアップしました。"

# デフォルトのオンボードオーディオを無効化
sudo sed -i -e 's/^dtparam=audio=on/#dtparam=audio=on/g' /boot/config.txt
echo "デフォルトオーディオ(dtparam=audio=on)を無効化しました。"

# I2SとDACのオーバーレイを有効化（既にあれば追記しない）
if ! grep -q "^dtparam=i2s=on$" /boot/config.txt; then
    echo "dtparam=i2s=on" | sudo tee -a /boot/config.txt
    echo "I2S(dtparam=i2s=on)を有効化しました。"
fi

if ! grep -q "^dtoverlay=hifiberry-dac$" /boot/config.txt; then
    echo "dtoverlay=hifiberry-dac" | sudo tee -a /boot/config.txt
    echo "DACオーバーレイ(dtoverlay=hifiberry-dac)を有効化しました。"
fi

# --- ALSAのデフォルト設定を作成 ---
echo ""
echo "--- ALSAのデフォルト設定ファイルを作成しています... ---"

# /etc/asound.conf を作成し、I2S DACをデフォルトのサウンドカードに設定
# hifiberry-dacオーバーレイを使用した場合、カード名は通常 'sndrpihifiberry' になります
cat << EOF | sudo tee /etc/asound.conf
pcm.!default {
    type hw
    card sndrpihifiberry
}
ctl.!default {
    type hw
    card sndrpihifiberry
}
EOF

echo "/etc/asound.conf を作成し、I2S DACをデフォルトに設定しました。"

# --- 完了メッセージ ---
echo ""
echo "--- セットアップが完了しました！ ---"
echo "設定を有効にするために、Raspberry Piを再起動する必要があります。"
echo "今すぐ再起動するには、'sudo reboot' を実行してください。"
