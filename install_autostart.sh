#!/bin/bash
# このスクリプトは setup_and_run.sh を初回起動時に自動実行するように設定します
# ラズパイ上で一度だけ実行してください

set -e

echo "========================================="
echo "Tsukimi Speaker 自動起動設定スクリプト"
echo "========================================="

# スクリプトのパスを設定
SETUP_SCRIPT="/home/tsukimi/tsukimi-speaker-rust/setup_and_run.sh"
RC_LOCAL="/etc/rc.local"

# setup_and_run.sh に実行権限を付与
echo "Step 1: setup_and_run.sh に実行権限を付与..."
sudo chmod +x "$SETUP_SCRIPT"
echo "✓ 実行権限を付与しました"

# rc.local が存在するか確認
if [ ! -f "$RC_LOCAL" ]; then
    echo "Step 2: /etc/rc.local を作成..."
    sudo tee "$RC_LOCAL" > /dev/null << 'EOF'
#!/bin/bash
# rc.local
#
# This script is executed at the end of each multiuser runlevel.
# Make sure that the script will "exit 0" on success or any other
# value on error.

exit 0
EOF
    sudo chmod +x "$RC_LOCAL"
    echo "✓ /etc/rc.local を作成しました"
else
    echo "Step 2: /etc/rc.local は既に存在します"
fi

# rc.local に setup_and_run.sh の呼び出しを追加
echo "Step 3: /etc/rc.local に自動起動設定を追加..."

# 既に追加されているかチェック
if grep -q "setup_and_run.sh" "$RC_LOCAL"; then
    echo "⚠️  既に設定されています。スキップします。"
else
    # exit 0 の前に追記
    sudo sed -i '/^exit 0/i \
# Tsukimi Speaker 自動セットアップ・起動\
/home/tsukimi/tsukimi-speaker-rust/setup_and_run.sh &\
' "$RC_LOCAL"
    echo "✓ 自動起動設定を追加しました"
fi

echo ""
echo "========================================="
echo "✓ 設定完了！"
echo "========================================="
echo ""
echo "次回起動時から自動的に以下が実行されます："
echo "  1. 初回起動時: セットアップ → 自動再起動"
echo "  2. 再起動後: プログラムが自動起動"
echo ""
echo "今すぐテストする場合は、以下を実行してください："
echo "  sudo $SETUP_SCRIPT"
echo ""

