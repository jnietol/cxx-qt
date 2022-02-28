// SPDX-FileCopyrightText: 2021 Klarälvdalens Datakonsult AB, a KDAB Group company <info@kdab.com>
// SPDX-FileContributor: Andrew Hayzen <andrew.hayzen@kdab.com>
//
// SPDX-License-Identifier: MIT OR Apache-2.0
use cxx_qt::make_qobject;

use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RequestCommand {
    Disconnect,
    Power { value: f64 },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Error,
    ErrorNoUuid,
    ErrorInvalidPower,
}

#[derive(Deserialize, Serialize)]
struct Request {
    command: RequestCommand,
    uuid: Option<Uuid>,
}

#[derive(Deserialize, Serialize)]
struct Response {
    status: Status,
    uuid: Option<Uuid>,
}

struct SensorData {
    power: f64,
    last_seen: SystemTime,
}

impl Default for SensorData {
    fn default() -> Self {
        Self {
            power: 0.0,
            last_seen: SystemTime::now(),
        }
    }
}

#[make_qobject]
mod energy_usage {
    use super::{Request, RequestCommand, Response, SensorData, Status};
    use async_std::{
        net::{TcpListener, TcpStream},
        prelude::*,
        task::spawn,
    };
    use futures::{
        channel::mpsc::{channel, Receiver, Sender},
        executor::block_on,
        stream::StreamExt,
    };
    use futures_timer::Delay;
    use std::{
        collections::HashMap,
        sync::{Arc, Condvar, Mutex},
        thread::JoinHandle,
        time::{Duration, SystemTime},
    };
    use uuid::Uuid;

    enum QtValueArrived {
        AverageUse(f64),
        Sensors(u32),
        TotalUsage(f64),
    }

    enum NetworkDataArrived {
        Disconnect(Uuid),
        Power(Uuid, f64),
    }

    pub struct Data {
        average_use: f64,
        sensors: u32,
        total_use: f64,
    }

    impl Default for Data {
        fn default() -> Self {
            Self {
                average_use: 0.0,
                sensors: 0,
                total_use: 0.0,
            }
        }
    }

    struct RustObj {
        qt_sender: Sender<QtValueArrived>,
        qt_receiver: Receiver<QtValueArrived>,
        join_handle_network: Option<JoinHandle<()>>,
        join_handle_processing: Option<JoinHandle<()>>,
        join_handle_timeout: Option<JoinHandle<()>>,
        join_handle_update: Option<JoinHandle<()>>,
    }

    impl Default for RustObj {
        fn default() -> Self {
            let (qt_sender, qt_receiver) = channel(4096);

            Self {
                qt_sender,
                qt_receiver,
                join_handle_network: None,
                join_handle_processing: None,
                join_handle_timeout: None,
                join_handle_update: None,
            }
        }
    }

    impl RustObj {
        async fn handle_connection(
            mut stream: TcpStream,
            mut event_sender: Sender<NetworkDataArrived>,
        ) {
            let mut buf = vec![0u8; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            let trimmed = std::str::from_utf8(&buf)
                .unwrap()
                .trim_matches(|c| c == ' ' || c == '\n' || c == '\r' || c == '\0');

            let response = match serde_json::from_str::<Request>(trimmed) {
                Ok(request) => match request.command {
                    RequestCommand::Disconnect => {
                        if let Some(uuid) = request.uuid {
                            event_sender
                                .try_send(NetworkDataArrived::Disconnect(uuid))
                                .unwrap();

                            Response {
                                status: Status::Ok,
                                uuid: Some(uuid),
                            }
                        } else {
                            Response {
                                status: Status::ErrorNoUuid,
                                uuid: None,
                            }
                        }
                    }
                    RequestCommand::Power { value } => {
                        if let Some(uuid) = request.uuid {
                            // Validate that our power is within the expected range
                            if !(0.0..=1000.0).contains(&value) {
                                Response {
                                    status: Status::ErrorInvalidPower,
                                    uuid: None,
                                }
                            } else {
                                event_sender
                                    .try_send(NetworkDataArrived::Power(uuid, value))
                                    .unwrap();

                                Response {
                                    status: Status::Ok,
                                    uuid: Some(uuid),
                                }
                            }
                        } else {
                            Response {
                                status: Status::ErrorNoUuid,
                                uuid: None,
                            }
                        }
                    }
                },
                Err(_) => Response {
                    status: Status::Error,
                    uuid: None,
                },
            };

            stream
                .write(serde_json::to_string(&response).unwrap().as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
        }

        #[invokable]
        fn start_server(&mut self, cpp: &mut CppObj) {
            if self.join_handle_network.is_some() || self.join_handle_processing.is_some() {
                println!("Already running a thread!");
                return;
            }

            // Here we start four threads with different tasks to manage sensors
            //
            // - Network thread
            //      - handles a TCP connection
            //      - validates values
            //      - writes items to a network queue
            // - Processing thread
            //      - reads the network queue
            //      - updates values in the sensors hashmap
            //      - notifies that sensors has changed
            // - Timeout thread
            //      - polls the sensors hashmap and checks for old data
            //      - notifies that sensors has changed
            // - Update thread
            //      - waits for sensors changed notification
            //      - reads the hashmap
            //      - writes new Qt values to qt queue

            let sensors_mutex = Arc::new(Mutex::new(HashMap::<Uuid, SensorData>::new()));

            let (network_sender, mut network_receiver) = channel(4096);
            let mut qt_sender = self.qt_sender.clone();
            let update_requester = cpp.update_requester();
            let update_pair = Arc::new((Mutex::new(false), Condvar::new()));

            // Prepare our timeout thread, if a sensor is not seen for 10 seconds we remove it
            let sensors_mutex_timeout = sensors_mutex.clone();
            let update_pair_timeout = update_pair.clone();
            let run_timeout = async move {
                loop {
                    Delay::new(Duration::from_millis(256)).await;

                    if let Ok(mut sensors) = sensors_mutex_timeout.try_lock() {
                        let sensors_count = sensors.len();
                        sensors.retain(|_, sensor| {
                            if let Ok(duration) = sensor.last_seen.elapsed() {
                                duration.as_secs() < 10
                            } else {
                                false
                            }
                        });

                        if sensors.len() < sensors_count {
                            let (lock, cvar) = &*update_pair_timeout;
                            if let Ok(mut changed) = lock.lock() {
                                *changed = true;
                                cvar.notify_one();
                            }
                        }
                    }
                }
            };
            // Prepare our processing thread which builds average, count, total
            let sensors_mutex_processing = sensors_mutex.clone();
            let update_pair_processing = update_pair.clone();
            let run_processing = async move {
                loop {
                    // TODO: instead block on channel or condition?
                    Delay::new(Duration::from_millis(8)).await;

                    let mut changed = false;

                    // Read our channel of sensor data from the network thread
                    while let Ok(event) = network_receiver.try_next() {
                        if let Some(event) = event {
                            if let Ok(mut sensors) = sensors_mutex_processing.lock() {
                                changed = true;

                                match event {
                                    NetworkDataArrived::Disconnect(uuid) => {
                                        sensors.remove(&uuid);
                                    }
                                    NetworkDataArrived::Power(uuid, value) => {
                                        let mut sensor = sensors.entry(uuid).or_default();
                                        sensor.power = value;
                                        sensor.last_seen = SystemTime::now();
                                    }
                                }
                            }
                        }
                    }

                    if changed {
                        let (lock, cvar) = &*update_pair_processing;
                        if let Ok(mut changed) = lock.lock() {
                            *changed = true;
                            cvar.notify_one();
                        }
                    }
                }
            };

            let sensors_mutex_update = sensors_mutex.clone();
            let update_pair_update = update_pair.clone();
            let run_update = async move {
                loop {
                    let (lock, cvar) = &*update_pair_update;
                    if let Ok(mut changed) = lock.lock() {
                        changed = cvar.wait(changed).unwrap();

                        if *changed {
                            if let Ok(sensors) = sensors_mutex_update.lock() {
                                // If there is new sensor info then build average, count, total and inform Qt
                                let total = sensors.values().fold(0.0, |acc, x| acc + x.power);
                                let count = sensors.len() as u32;
                                let average = if count > 0 {
                                    total / (count as f64)
                                } else {
                                    0.0
                                };

                                qt_sender
                                    .try_send(QtValueArrived::TotalUsage(total))
                                    .unwrap();
                                qt_sender.try_send(QtValueArrived::Sensors(count)).unwrap();
                                qt_sender
                                    .try_send(QtValueArrived::AverageUse(average))
                                    .unwrap();

                                update_requester.request_update();
                            }

                            *changed = false;
                        }
                    }
                }
            };

            // Prepare our Tcp server which listens for sensors
            let run_server = async move {
                let listener = TcpListener::bind("127.0.0.1:8080").await.unwrap();
                listener
                    .incoming()
                    .map(|stream| (stream, network_sender.clone()))
                    .for_each_concurrent(
                        /* limit */ None,
                        |(stream, network_sender)| async move {
                            let stream = stream.unwrap();
                            spawn(RustObj::handle_connection(stream, network_sender));
                        },
                    )
                    .await;
            };

            // Start our threads
            self.join_handle_processing =
                Some(std::thread::spawn(move || block_on(run_processing)));
            self.join_handle_timeout = Some(std::thread::spawn(move || block_on(run_timeout)));
            self.join_handle_network = Some(std::thread::spawn(move || block_on(run_server)));
            self.join_handle_update = Some(std::thread::spawn(move || block_on(run_update)));
        }
    }

    impl UpdateRequestHandler<CppObj<'_>> for RustObj {
        fn handle_update_request(&mut self, cpp: &mut CppObj) {
            // Process each of the update requests from the background thread
            while let Ok(event) = self.qt_receiver.try_next() {
                if let Some(event) = event {
                    match event {
                        QtValueArrived::AverageUse(average) => cpp.set_average_use(average),
                        QtValueArrived::Sensors(sensors) => cpp.set_sensors(sensors),
                        QtValueArrived::TotalUsage(total) => cpp.set_total_use(total),
                    }
                }
            }
        }
    }
}