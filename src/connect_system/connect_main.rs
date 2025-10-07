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

/// place_typeに基づいてサウンドファイル名を決定する
fn get_sound_file_from_place_type(place_type: &str) -> &'static str {
    match place_type {
        "projection_mapping" => "tsukimi-hotoke.mp3",
        "buddhas_bowl" => "tsukimi-hotoke.mp3",
        "jeweled_branch" => "tsukimi-hotoke.mp3",
        "fire_rat_robe" => "tsukimi-hotoke.mp3",
        "dragons_jewel" => "tsukimi-hotoke.mp3",
        "swallows_cowry" => "tsukimi-hotoke.mp3",
        _ => "tsukimi-main.mp3",
    }
}

#[instrument(skip(client, rx, sound_map))]
async fn run_device_service_client(
    mut client: DeviceServiceClient<Channel>,
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    sound_setting_tx: mpsc::Sender<SoundSetting>,
    sound_map: Arc<Mutex<HashMap<String, String>>>,
    my_address: Arc<Mutex<Option<String>>>,
    current_points: Arc<Mutex<i32>>,
) {
    info!("Starting DeviceService client...");
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
        .chunks_timeout(10, Duration::from_millis(50)) // 100ms → 50ms に短縮
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
                                    // TODO: Handle TimeUpdate
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

                                    info!(new_sound_map_size = sound_map.len(), ?sound_map, "Updated sound_map with differential update");
                                }
                                Event::PointUpdate(point_update) => {
                                    debug!(?point_update, "PointUpdate received");
                                    let my_address_guard = my_address.lock().unwrap();
                                    if let Some(my_addr) = my_address_guard.as_ref() {
                                        if *my_addr == point_update.user_id {
                                            let mut points = current_points.lock().unwrap();
                                            *points = point_update.points;
                                            info!(user_id = %point_update.user_id, points = *points, "Updated my points");
                                        } else {
                                            debug!(
                                                my_addr,
                                                received_user_id = %point_update.user_id,
                                                "Received points for another user, ignoring."
                                            );
                                        }
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

#[instrument(skip(rx, time_sync_tx, sound_map))]
pub async fn connect_main(
    rx: broadcast::Receiver<Arc<DeviceInfo>>,
    time_sync_tx: mpsc::Sender<u64>,
    sound_setting_tx: mpsc::Sender<SoundSetting>,
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
        tokio::spawn(run_device_service_client(
            device_client,
            rx,
            sound_setting_tx_clone,
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