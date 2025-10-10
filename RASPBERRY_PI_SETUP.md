# Tsukimi Speaker - ラズパイ自動セットアップガイド

## 📦 ファイル構成

このプロジェクトには3つの自動化スクリプトがあります：

### 1. `setup_and_run.sh` - メインスクリプト
セットアップとプログラム実行を行うメインスクリプト。
- 初回: 環境セットアップ → 自動再起動
- 2回目以降: プログラム起動

### 2. `install_autostart.sh` - rc.local方式（推奨）
**最もシンプルで確実な方法**
```bash
sudo bash install_autostart.sh
```

### 3. `install_autostart_systemd.sh` - systemd方式
より現代的な方法
```bash
sudo bash install_autostart_systemd.sh
```

## 🚀 セットアップ手順

### ステップ1: ラズパイにファイルを配置

SDカードまたはSSH経由で、以下のようにファイルを配置：

```
/home/tsukimi/tsukimi-speaker-rust/
  ├── setup_and_run.sh
  ├── install_autostart.sh         # どちらか選択
  ├── install_autostart_systemd.sh # どちらか選択
  ├── src/
  ├── Cargo.toml
  ├── *.mp3  (全BGM・SEファイル)
  └── その他プロジェクトファイル
```

### ステップ2: 自動起動を設定

**方法A: rc.local方式（推奨）**
```bash
cd /home/tsukimi/tsukimi-speaker-rust
sudo bash install_autostart.sh
```

**方法B: systemd方式**
```bash
cd /home/tsukimi/tsukimi-speaker-rust
sudo bash install_autostart_systemd.sh
```

### ステップ3: 再起動

```bash
sudo reboot
```

## 🎯 動作の流れ

### 初回起動時
1. ラズパイ起動
2. `setup_and_run.sh` が自動実行
3. システムアップデート
4. 必要なパッケージインストール
5. Rustインストール
6. プログラムビルド
7. Bluetooth/PulseAudio設定
8. systemdサービス登録
9. **自動再起動**

### 2回目以降（再起動後）
1. ラズパイ起動
2. systemdサービスが `tsukimi-speaker` を自動起動
3. プログラム実行中

## 📝 便利なコマンド

### サービスの状態確認
```bash
sudo systemctl status tsukimi-speaker.service
```

### ログの確認
```bash
# セットアップログ
cat /home/tsukimi/tsukimi_setup.log

# プログラムログ（systemd）
sudo journalctl -u tsukimi-speaker.service -f
```

### サービスの制御
```bash
# 停止
sudo systemctl stop tsukimi-speaker.service

# 起動
sudo systemctl start tsukimi-speaker.service

# 再起動
sudo systemctl restart tsukimi-speaker.service

# 自動起動を無効化
sudo systemctl disable tsukimi-speaker.service
```

### 手動でセットアップをやり直す
```bash
# セットアップフラグを削除
rm /home/tsukimi/.tsukimi_setup_complete

# 再度セットアップ実行
sudo /home/tsukimi/tsukimi-speaker-rust/setup_and_run.sh
```

## 🔧 トラブルシューティング

### プログラムが起動しない場合

1. **ログを確認**
   ```bash
   sudo journalctl -u tsukimi-speaker.service -n 50
   ```

2. **手動で実行してエラーを確認**
   ```bash
   cd /home/tsukimi/tsukimi-speaker-rust
   ./target/release/tsukimi-speaker
   ```

3. **Bluetoothが動作しているか確認**
   ```bash
   sudo systemctl status bluetooth
   bluetoothctl
   ```

### セットアップが途中で止まった場合

1. **セットアップログを確認**
   ```bash
   tail -n 100 /home/tsukimi/tsukimi_setup.log
   ```

2. **フラグを削除して再実行**
   ```bash
   rm /home/tsukimi/.tsukimi_setup_complete
   sudo reboot
   ```

## 📋 必要なファイル一覧

### BGMファイル（30個）
- tsukimi-main_1.mp3 ~ tsukimi-main_5.mp3
- tsukimi-hotoke_1.mp3 ~ tsukimi-hotoke_5.mp3
- tsukimi-eda_1.mp3 ~ tsukimi-eda_5.mp3
- tsukimi-kai_1.mp3 ~ tsukimi-kai_5.mp3
- tsukimi-nezumi_1.mp3 ~ tsukimi-nezumi_5.mp3
- tsukimi-ryu_1.mp3 ~ tsukimi-ryu_5.mp3

### SEファイル（5個）
- se-activation.mp3
- se-point.mp3
- se-nezumi.mp3
- se-hotoke.mp3
- interaction-se-fire.mp3

## ⚙️ 設定ファイルの場所

- **プログラムディレクトリ**: `/home/tsukimi/tsukimi-speaker-rust/`
- **セットアップフラグ**: `/home/tsukimi/.tsukimi_setup_complete`
- **セットアップログ**: `/home/tsukimi/tsukimi_setup.log`
- **systemdサービス**: `/etc/systemd/system/tsukimi-speaker.service`
- **自動起動設定（rc.local）**: `/etc/rc.local`
- **自動起動設定（systemd）**: `/etc/systemd/system/tsukimi-setup.service`

## 🎉 完了！

これで、ラズパイの電源を入れるだけで自動的にセットアップ＆起動されます！

