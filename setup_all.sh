#!/bin/bash
set -e

# setup_dependencies.sh
if [ -f "setup_dependencies.sh" ]; then
  echo "--- 依存関係のセットアップを開始します ---"
  bash setup_dependencies.sh
  echo "--- 依存関係のセットアップが完了しました ---"
else
  echo "setup_dependencies.sh が見つかりません"
  exit 1
fi

# setup_max98357a.sh
if [ -f "setup_max98357a.sh" ]; then
  echo "--- MAX98357A I2S DAC のセットアップを開始します ---"
  bash setup_max98357a.sh
  echo "--- MAX98357A I2S DAC のセットアップが完了しました ---"
else
  echo "setup_max98357a.sh が見つかりません"
  exit 1
fi

# setup_and_run.sh
if [ -f "setup_and_run.sh" ]; then
  echo "--- プロジェクトのセットアップと実行を開始します ---"
  bash setup_and_run.sh
  echo "--- プロジェクトのセットアップと実行が完了しました ---"
else
  echo "setup_and_run.sh が見つかりません"
  exit 1
fi

echo "--- すべてのセットアップが完了しました ---"
