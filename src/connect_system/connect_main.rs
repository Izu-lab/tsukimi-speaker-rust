use crate::proto::proto::device_service_client::DeviceServiceClient;
use crate::proto::proto::stream_device_info_response::Event;
use crate::proto::proto::time_service_client::TimeServiceClient;
use crate::proto::proto::{LocationRssi, SoundSetting, StreamDeviceInfoRequest, StreamTimeRequest};
use crate::DeviceInfo;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Endpoint};
use tracing::{debug, error, info, instrument, warn};
use serde::{Deserialize, Serialize};

// システム有効化状態を管理するメッセージ
#[derive(Debug, Clone)]
pub struct SystemEnabledState {
    pub enabled: bool,
}

// インタラクション用の構造体
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InteractionRequest {
    location_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InteractionResponse {
    success: bool,
    message: String,
}

// インタラクション状態管理
struct InteractionState {
    last_interaction_time: HashMap<String, std::time::Instant>,
    interaction_cooldown: Duration,
}

impl InteractionState {
    fn new() -> Self {
        Self {
            last_interaction_time: HashMap::new(),
            interaction_cooldown: Duration::from_secs(10), // 10秒間のクールダウン
        }
    }

    fn can_interact(&mut self, place_type: &str) -> bool {
        let now = std::time::Instant::now();
        if let Some(&last_time) = self.last_interaction_time.get(place_type) {
            if now.duration_since(last_time) < self.interaction_cooldown {
                return false;
            }
        }
        self.last_interaction_time.insert(place_type.to_string(), now);
        true
    }
}

/// place_typeに基づいてサウンドファイル名を決定する
fn get_sound_file_from_place_type(place_type: &str) -> &'static str {
    match place_type {
        "projection_mapping" => "tsukimi-main.mp3",
        "buddhas_bowl" => "tsukimi-hotoke.mp3",
        "jeweled_branch" => "tsukimi-eda.mp3",
        "fire_rat_robe" => "tsukimi-nezumi.mp3",
        "dragons_jewel" => "tsukimi-ryu.mp3",
        "swallows_cowry" => "tsukimi-kai.mp3",
        _ => "tsukimi-main.mp3",
    }
}

/// place_typeに基づいてSEファイル名を決定する
fn get_se_file_from_place_type(place_type: &str) -> Option<&'static str> {
    match place_type {
        "fire_rat_robe" => Some("interaction-se-fire.mp3"),
        "buddhas_bowl" => Some("interaction-se-fire.mp3"),
        _ => None,
    }
}

/// インタラクション可能なplace_typeかどうかを判定
fn is_interactive_place_type(place_type: &str) -> bool {
    matches!(place_type, "fire_rat_robe" | "buddhas_bowl")
}

/// インタラクションAPIを呼び出す
#[allow(dead_code)]
async fn send_interaction_request(user_id: String, place_type: String) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let request = InteractionRequest {
        location_type: place_type.clone(),
    };

    // エンドポイントURLを構築: https://tsukimi.paon.dev/players/{user_id}/increment
    let url = format!("https://tsukimi.paon.dev/players/{}/increment", user_id);

    info!(?request, url = %url, "Sending interaction request");

    match client
        .post(&url)
        .json(&request)
        .timeout(Duration::from_secs(5))
        .send()
        .await
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<InteractionResponse>().await {
                    Ok(data) => {
                        info!(?data, "Interaction request successful");
                    }
                    Err(e) => {
                        warn!("Failed to parse interaction response: {}", e);
                    }
                }
            } else {
                warn!("Interaction request failed with status: {}", response.status());
            }
        }
        Err(e) => {
            error!("Failed to send interaction request: {}", e);
        }
    }

    Ok(())
}


#[instrument(skip(client, rx, sound_map, se_tx, system_enabled_tx))]
async fn run_device_service_client(
    mut client: DeviceServiceClient<Channel>,
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    sound_setting_tx: mpsc::Sender<SoundSetting>,
    se_tx: mpsc::Sender<crate::audio_system::audio_main::SePlayRequest>,
    system_enabled_tx: broadcast::Sender<SystemEnabledState>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) {
    info!("Starting DeviceService client...");

    // インタラクション状態管理
    let interaction_state = Arc::new(Mutex::new(InteractionState::new()));

    // ロケーション情報のキャッシュ（address -> place_type）
    let location_place_types = Arc::new(Mutex::new(HashMap::<String, String>::new()));

    // RSSI閾値（この値を上回ったらインタラクション発動）
    // RSSIは0に近いほど距離が近い（例: -35より-30の方が近い）
    const INTERACTION_RSSI_THRESHOLD: i16 = -35;

    // デバイス情報を監視するためのRxクローン
    let mut interaction_rx = rx.resubscribe();

    // インタラクション検知タスクを起動
    let my_address_for_interaction = Arc::clone(&my_address);
    let location_place_types_for_interaction = Arc::clone(&location_place_types);
    let interaction_state_for_task = Arc::clone(&interaction_state);
    let se_tx_for_interaction = se_tx.clone();

    tokio::spawn(async move {
        let mut last_rssi_map: HashMap<String, i16> = HashMap::new();

        loop {
            match interaction_rx.recv().await {
                Ok(device_info) => {
                    // 前回のRSSIを取得
                    let prev_rssi = last_rssi_map.get(&device_info.address).copied().unwrap_or(i16::MIN);
                    let current_rssi = device_info.rssi;

                    // RSSI閾値を上回った場合（0に近づいた = 近づいた場合）
                    if prev_rssi <= INTERACTION_RSSI_THRESHOLD && current_rssi > INTERACTION_RSSI_THRESHOLD {
                        info!(
                            address = %device_info.address,
                            rssi = current_rssi,
                            threshold = INTERACTION_RSSI_THRESHOLD,
                            "Device came very close (RSSI > {}), checking for interaction", INTERACTION_RSSI_THRESHOLD
                        );

                        // place_typeを取得
                        let place_type = {
                            let location_types = location_place_types_for_interaction.lock().unwrap();
                            location_types.get(&device_info.address).cloned()
                        };

                        if let Some(place_type) = place_type {
                            // インタラクション可能な場所かチェック
                            if is_interactive_place_type(&place_type) {
                                let can_interact = {
                                    let mut state = interaction_state_for_task.lock().unwrap();
                                    state.can_interact(&place_type)
                                };

                                if can_interact {
                                    info!(
                                        place_type = %place_type,
                                        address = %device_info.address,
                                        rssi = current_rssi,
                                        "Triggering interaction"
                                    );

                                    // SEファイルを取得してaudio_mainに送信
                                    if let Some(se_file) = get_se_file_from_place_type(&place_type) {
                                        let se_request = crate::audio_system::audio_main::SePlayRequest {
                                            file_path: se_file.to_string(),
                                        };

                                        if let Err(e) = se_tx_for_interaction.send(se_request).await {
                                            error!("Failed to send SE play request: {}", e);
                                        } else {
                                            info!("SE play request sent successfully");
                                        }
                                    }

                                    // インタラクションAPIを呼び出し
                                    let user_id_opt = my_address_for_interaction.lock().unwrap().clone();
                                    if let Some(user_id) = user_id_opt {
                                        if let Err(e) = send_interaction_request(user_id, place_type).await {
                                            error!("Failed to send interaction request: {}", e);
                                        }
                                    }
                                } else {
                                    debug!(
                                        place_type = %place_type,
                                        "Interaction still in cooldown"
                                    );
                                }
                            }
                        }
                    }

                    last_rssi_map.insert(device_info.address.clone(), current_rssi);
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(skipped, "Interaction receiver lagged");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("Interaction receiver closed");
                    break;
                }
            }
        }
    });

    let sound_map_for_filter = Arc::clone(&sound_map);
    let my_address_for_stream = Arc::clone(&my_address);
    let device_info_stream = BroadcastStream::new(rx)
        .filter_map(move |result| {
            let sound_map = Arc::clone(&sound_map_for_filter);
            result.ok().and_then(|info| {
                let sound_map = sound_map.lock().unwrap();
                if sound_map.contains_key(&info.address) {
                    Some(info)
                } else {
                    None
                }
            })
        })
        .chunks_timeout(10, Duration::from_millis(50))
        .map(move |infos| {
            let locations: Vec<LocationRssi> = infos
                .into_iter()
                .map(|info| LocationRssi {
                    address: info.address.clone(),
                    rssi: info.rssi as i32,
                })
                .collect();

            let user_id = my_address_for_stream
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "".to_string());

            info!(
                ?locations,
                %user_id,
                locations_count = locations.len(),
                "Sending device info to server"
            );
            StreamDeviceInfoRequest { user_id, locations }
        });

    match client.stream_device_info(device_info_stream).await {
        Ok(response) => {
            info!("DeviceService connected. Waiting for responses...");
            let mut stream = response.into_inner();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(res) => {
                        if let Some(event) = res.event {
                            match event {
                                Event::TimeUpdate(time_update) => {
                                    debug!(?time_update, "TimeUpdate received");
                                }
                                Event::LocationUpdate(location_update) => {
                                    info!(?location_update, "LocationUpdate received");
                                    let mut sound_map = sound_map.lock().unwrap();
                                    info!(old_sound_map_size = sound_map.len(), "Before updating sound_map");

                                    // 差分更新：新しいロケーションをマップに格納
                                    let mut new_addresses = std::collections::HashSet::new();
                                    for loc in &location_update.locations {
                                        new_addresses.insert(loc.address.clone());
                                        let sound_file = get_sound_file_from_place_type(&loc.place_type);

                                        // place_typeをキャッシュ（インタラクション検知用）
                                        {
                                            let mut location_types = location_place_types.lock().unwrap();
                                            location_types.insert(loc.address.clone(), loc.place_type.clone());
                                        }

                                        info!(
                                            address = %loc.address,
                                            place_type = %loc.place_type,
                                            sound_file = %sound_file,
                                            "Processing location entry"
                                        );
                                        if !sound_file.is_empty() {
                                            sound_map.insert(loc.address.clone(), sound_file.to_string());
                                        } else {
                                            warn!(
                                                address = %loc.address,
                                                place_type = %loc.place_type,
                                                "Skipping empty sound file for location"
                                            );
                                        }
                                    }

                                    // 新しいリストに存在しないアドレスを削除
                                    sound_map.retain(|addr, _| new_addresses.contains(addr));

                                    // location_place_typesも同期
                                    {
                                        let mut location_types = location_place_types.lock().unwrap();
                                        location_types.retain(|addr, _| new_addresses.contains(addr));
                                    }

                                    info!(new_sound_map_size = sound_map.len(), ?sound_map, "Updated sound_map with differential update");
                                }
                                Event::PointUpdate(point_update) => {
                                    debug!(?point_update, "PointUpdate received");

                                    // user_idの比較を先にして、MutexGuardをすぐに解放
                                    let is_my_address = {
                                        let my_address_guard = my_address.lock().unwrap();
                                        my_address_guard.as_ref().map(|addr| *addr == point_update.user_id).unwrap_or(false)
                                    };

                                    if is_my_address {
                                        let old_points = {
                                            let points = current_points.lock().unwrap();
                                            *points
                                        };

                                        {
                                            let mut points = current_points.lock().unwrap();
                                            *points = point_update.points;
                                        }

                                        let new_points = point_update.points;
                                        info!(user_id = %point_update.user_id, points = new_points, old_points = old_points, "Updated my points");

                                        // ポイントが増えた場合に音を再生
                                        if new_points > old_points {
                                            info!(points_gained = new_points - old_points, "Points increased! Playing sound effect");
                                            let se_request = crate::audio_system::audio_main::SePlayRequest {
                                                file_path: "interaction-se-fire.mp3".to_string(),
                                            };
                                            if let Err(e) = se_tx.send(se_request).await {
                                                error!("Failed to send SE play request for point gain: {}", e);
                                            } else {
                                                info!("Point gain SE play request sent successfully");
                                            }
                                        }
                                    } else {
                                        debug!(
                                            received_user_id = %point_update.user_id,
                                            "Received points for another user, ignoring."
                                        );
                                    }
                                }
                                Event::SoundSettingUpdate(sound_setting_update) => {
                                    debug!(?sound_setting_update, "SoundSettingUpdate received");
                                    if let Some(settings) = sound_setting_update.settings {
                                        if let Err(e) = sound_setting_tx.send(settings).await {
                                            error!("Failed to send sound settings: {}", e);
                                        }
                                    }
                                }
                                Event::MoonlightUpdate(moonlight_update) => {
                                    info!(?moonlight_update, "MoonlightUpdate received");

                                    // 自分のデバイスのenabledフラグを確認
                                    let my_device_id = my_address.lock().unwrap().clone();
                                    if let Some(device_id) = my_device_id {
                                        // moonlightsリストから自分のデバイスを探す
                                        for moonlight in &moonlight_update.moonlights {
                                            if moonlight.device == device_id || moonlight.address == device_id {
                                                info!(
                                                    device = %moonlight.device,
                                                    address = %moonlight.address,
                                                    enabled = moonlight.enabled,
                                                    "Found my device in MoonlightUpdate"
                                                );

                                                let state = SystemEnabledState {
                                                    enabled: moonlight.enabled,
                                                };

                                                if let Err(e) = system_enabled_tx.send(state) {
                                                    error!("Failed to send system enabled state: {}", e);
                                                } else {
                                                    info!(enabled = moonlight.enabled, "System enabled state sent successfully");
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => error!("DeviceService stream error: {}", e),
                }
            }
        }
        Err(e) => {
            error!("Failed to connect to DeviceService: {}", e);
        }
    }
}

#[instrument(skip(client, time_sync_tx))]
async fn run_time_service_client(
    mut client: TimeServiceClient<Channel>,
    time_sync_tx: mpsc::Sender<u64>,
) {
    info!("Starting TimeService client...");
    let request = tonic::Request::new(StreamTimeRequest {});
    match client.stream_time(request).await {
        Ok(response) => {
            info!("TimeService connected. Waiting for responses...");
            let mut stream = response.into_inner();
            while let Some(item) = stream.next().await {
                match item {
                    Ok(res) => {
                        debug!(?res, "Received time from server");
                        if let Err(e) = time_sync_tx.send(res.elapsed_nanoseconds as u64).await {
                            error!("Failed to send time sync data: {}", e);
                        }
                    }
                    Err(e) => error!("TimeService stream error: {}", e),
                }
            }
        }
        Err(e) => {
            error!("Failed to connect to TimeService: {}", e);
        }
    }
}

#[instrument(skip(rx, time_sync_tx, sound_map, se_tx, system_enabled_tx))]
pub async fn connect_main(
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    time_sync_tx: mpsc::Sender<u64>,
    sound_setting_tx: mpsc::Sender<SoundSetting>,
    se_tx: mpsc::Sender<crate::audio_system::audio_main::SePlayRequest>,
    system_enabled_tx: broadcast::Sender<SystemEnabledState>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) -> anyhow::Result<()> {
    let server_addr = "http://35.221.123.49:50051";
    info!("Connecting to gRPC server at {}", server_addr);

    // サーバーに接続できるまでリトライ
    let channel = loop {
        match Endpoint::from_static(server_addr)
            .connect_timeout(Duration::from_secs(5))
            .connect()
            .await
        {
            Ok(channel) => {
                info!("Successfully connected to gRPC server.");
                break channel;
            }
            Err(e) => {
                error!(
                    "Failed to connect to server: {:?}. Retrying in 5 seconds...",
                    e
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            }
        }
    };

    // DeviceServiceクライアント
    let device_client = DeviceServiceClient::new(channel.clone());

    // TimeServiceクライアント
    let time_client = TimeServiceClient::new(channel);

    info!("Spawning gRPC client tasks...");
    let device_service_handle = {
        let sound_map_clone = Arc::clone(&sound_map);
        let my_address_clone = Arc::clone(&my_address);
        let current_points_clone = Arc::clone(&current_points);
        let sound_setting_tx_clone = sound_setting_tx.clone();
        let se_tx_clone = se_tx.clone();
        let system_enabled_tx_clone = system_enabled_tx.clone();
        tokio::spawn(run_device_service_client(
            device_client,
            rx,
            sound_setting_tx_clone,
            se_tx_clone,
            system_enabled_tx_clone,
            sound_map_clone,
            my_address_clone,
            current_points_clone,
        ))
    };
    let time_service_handle = tokio::spawn(run_time_service_client(time_client, time_sync_tx));

    // 両方のタスクが終了するのを待つ
    if let Err(e) = tokio::try_join!(device_service_handle, time_service_handle) {
        error!("gRPC client task failed: {}", e);
    }

    info!("gRPC client tasks finished.");
    Ok(())
}
