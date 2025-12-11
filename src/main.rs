/*
 *
 * Copyright (c) 2020-2022 Project CHIP Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 * http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! An example Matter device that implements a simple On/Off Light over Ethernet,
//! controlling a Linux backlight device.
#![allow(clippy::uninlined_format_args)]

use core::cell::Cell;
use core::pin::pin;

use std::fs;
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::path::PathBuf;

use embassy_futures::select::{select3, select4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use async_signal::{Signal, Signals};
// REMOVED 'trace' to fix the warning
use log::{error, info}; 

use futures_lite::StreamExt;

use rs_matter::dm::clusters::decl::on_off as on_off_cluster;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::net_comm::NetworkType;
// ADDED NoLevelControl import here
use rs_matter::dm::clusters::on_off::NoLevelControl; 
use rs_matter::dm::clusters::on_off::{self, OnOffHooks, StartUpOnOffEnum};
use rs_matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::devices::DEV_TYPE_ON_OFF_LIGHT;
use rs_matter::dm::endpoints;
use rs_matter::dm::networks::unix::UnixNetifs;
use rs_matter::dm::subscriptions::DefaultSubscriptions;
use rs_matter::dm::IMBuffer;
use rs_matter::dm::{
    Async, AsyncHandler, AsyncMetadata, Cluster, DataModel, Dataver, EmptyHandler, Endpoint,
    EpClMatcher, Node,
};
use rs_matter::error::{Error, ErrorCode};
use rs_matter::pairing::qr::QrTextType;
use rs_matter::pairing::DiscoveryCapabilities;
use rs_matter::persist::{Psm, NO_NETWORKS};
use rs_matter::respond::DefaultResponder;
use rs_matter::sc::pake::MAX_COMM_WINDOW_TIMEOUT_SECS;
use rs_matter::tlv::Nullable;
use rs_matter::transport::MATTER_SOCKET_BIND_ADDR;
use rs_matter::utils::init::InitMaybeUninit;
use rs_matter::utils::select::Coalesce;
use rs_matter::utils::storage::pooled::PooledBuffers;
use rs_matter::{clusters, devices, with, Matter, MATTER_PORT};

use static_cell::StaticCell;

#[path = "./mdns.rs"]
mod mdns;

// Statically allocate in BSS the bigger objects
static MATTER: StaticCell<Matter> = StaticCell::new();
static BUFFERS: StaticCell<PooledBuffers<10, NoopRawMutex, IMBuffer>> = StaticCell::new();
static SUBSCRIPTIONS: StaticCell<DefaultSubscriptions> = StaticCell::new();
static PSM: StaticCell<Psm<4096>> = StaticCell::new();

#[cfg(feature = "chip-test")]
const PERSIST_FILE_NAME: &str = "/tmp/chip_kvs";

fn main() -> Result<(), Error> {
    let thread = std::thread::Builder::new()
        .stack_size(550 * 1024)
        .spawn(run)
        .unwrap();

    thread.join().unwrap()
}

fn run() -> Result<(), Error> {
    #[cfg(not(feature = "chip-test"))]
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    #[cfg(feature = "chip-test")]
    env_logger::builder()
        .format(|buf, record| {
            use std::io::Write;
            writeln!(buf, "{}: {}", record.level(), record.args())
        })
        .target(env_logger::Target::Stdout)
        .filter_level(::log::LevelFilter::Debug)
        .init();

    info!(
        "Matter memory: Matter (BSS)={}B, IM Buffers (BSS)={}B, Subscriptions (BSS)={}B",
        core::mem::size_of::<Matter>(),
        core::mem::size_of::<PooledBuffers<10, NoopRawMutex, IMBuffer>>(),
        core::mem::size_of::<DefaultSubscriptions>()
    );

    let matter = MATTER.uninit().init_with(Matter::init(
        &TEST_DEV_DET,
        TEST_DEV_COMM,
        &TEST_DEV_ATT,
        rs_matter::utils::epoch::sys_epoch,
        rs_matter::utils::rand::sys_rand,
        MATTER_PORT,
    ));

    matter.initialize_transport_buffers()?;

    let buffers = BUFFERS.uninit().init_with(PooledBuffers::init(0));

    let subscriptions = SUBSCRIPTIONS
        .uninit()
        .init_with(DefaultSubscriptions::init());

    // OnOff cluster setup
    // Compiler infers the second generic type is NoLevelControl because of dm_handler signature
    let on_off_handler =
        on_off::OnOffHandler::new(Dataver::new_rand(matter.rand()), 1, OnOffDeviceLogic::new());

    // Create the Data Model instance
    let dm = DataModel::new(
        matter,
        buffers,
        subscriptions,
        dm_handler(matter, &on_off_handler),
    );

    let responder = DefaultResponder::new(&dm);
    info!(
        "Responder memory: Responder (stack)={}B, Runner fut (stack)={}B",
        core::mem::size_of_val(&responder),
        core::mem::size_of_val(&responder.run::<4, 4>())
    );

    let mut respond = pin!(responder.run::<4, 4>());
    let mut dm_job = pin!(dm.run());

    let socket = async_io::Async::<UdpSocket>::bind(MATTER_SOCKET_BIND_ADDR)?;

    info!(
        "Transport memory: Transport fut (stack)={}B, mDNS fut (stack)={}B",
        core::mem::size_of_val(&matter.run(&socket, &socket)),
        core::mem::size_of_val(&mdns::run_mdns(matter))
    );

    let mut mdns = pin!(mdns::run_mdns(matter));
    let mut transport = pin!(matter.run(&socket, &socket));

    let psm = PSM.uninit().init_with(Psm::init());
    #[cfg(not(feature = "chip-test"))]
    let path = std::env::temp_dir().join("rs-matter");
    #[cfg(feature = "chip-test")]
    let path = PathBuf::from(PERSIST_FILE_NAME);

    info!(
        "Persist memory: Persist (BSS)={}B, Persist fut (stack)={}B, Persist path={}",
        core::mem::size_of::<Psm<4096>>(),
        core::mem::size_of_val(&psm.run(&path, matter, NO_NETWORKS)),
        path.as_path().to_str().unwrap_or("none")
    );

    psm.load(&path, matter, NO_NETWORKS)?;

    matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;

    if !matter.is_commissioned() {
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS)?;
    }

    let mut persist = pin!(psm.run(&path, matter, NO_NETWORKS));

    let mut term_signal = Signals::new([Signal::Term])?;
    let mut term = pin!(async {
        term_signal.next().await;
        Ok(())
    });

    let all = select4(
        &mut transport,
        &mut mdns,
        &mut persist,
        select3(&mut respond, &mut dm_job, &mut term).coalesce(),
    );

    futures_lite::future::block_on(all.coalesce())
}

/// The Node meta-data describing our Matter device.
const NODE: Node<'static> = Node {
    id: 0,
    endpoints: &[
        endpoints::root_endpoint(NetworkType::Ethernet),
        Endpoint {
            id: 1,
            device_types: devices!(DEV_TYPE_ON_OFF_LIGHT),
            clusters: clusters!(
                desc::DescHandler::CLUSTER,
                OnOffDeviceLogic::CLUSTER,
            ),
        },
    ],
};

/// The Data Model handler + meta-data for our Matter device.
fn dm_handler<'a, OH: OnOffHooks>(
    matter: &'a Matter<'a>,
    // CHANGED: Use NoLevelControl instead of ()
    on_off: &'a on_off::OnOffHandler<'a, OH, NoLevelControl>, 
) -> impl AsyncMetadata + AsyncHandler + 'a {
    (
        NODE,
        endpoints::with_eth(
            &(),
            &UnixNetifs,
            matter.rand(),
            endpoints::with_sys(
                &false,
                matter.rand(),
                EmptyHandler
                    .chain(
                        EpClMatcher::new(Some(1), Some(desc::DescHandler::CLUSTER.id)),
                        Async(desc::DescHandler::new(Dataver::new_rand(matter.rand())).adapt()),
                    )
                    .chain(
                        EpClMatcher::new(Some(1), Some(OnOffDeviceLogic::CLUSTER.id)),
                        on_off::HandlerAsyncAdaptor(on_off),
                    ),
            ),
        ),
    )
}

// --- START: Backlight Control Configuration ---

// **IMPORTANT:** Replace with your actual path.
const BACKLIGHT_DEVICE_PATH: &str = "/sys/class/backlight/intel_backlight";
const BRIGHTNESS_FILE: &str = "brightness";
const MAX_BRIGHTNESS_FILE: &str = "max_brightness";

// Helper to read max brightness
fn get_max_backlight() -> Result<u32, Error> {
    let path = PathBuf::from(BACKLIGHT_DEVICE_PATH).join(MAX_BRIGHTNESS_FILE);
    fs::read_to_string(path)
        .map_err(|e| {
            error!("Failed to read max_brightness: {}", e);
            rs_matter::error::Error::from(ErrorCode::Failure)
        })?
        .trim()
        .parse::<u32>()
        .map_err(|e| {
            error!("Failed to parse max_brightness: {}", e);
            rs_matter::error::Error::from(ErrorCode::Failure)
        })
}

// --- END: Backlight Control Configuration ---

// Implementing the OnOff business logic

#[derive(Default)]
struct OnOffPersistentState {
    on_off: bool,
    start_up_on_off: Option<StartUpOnOffEnum>,
}

impl OnOffPersistentState {
    fn to_bytes_from_values(on_off: bool, start_up_on_off: Option<StartUpOnOffEnum>) -> u8 {
        let on_off = on_off as u8;
        let start_up_on_off: u8 = match start_up_on_off {
            Some(StartUpOnOffEnum::Off) => 0,
            Some(StartUpOnOffEnum::On) => 1,
            Some(StartUpOnOffEnum::Toggle) => 2,
            None => 3,
        };
        on_off + (start_up_on_off << 1)
    }

    fn from_bytes(data: u8) -> Result<Self, Error> {
        Ok(Self {
            on_off: data & 1 != 0,
            start_up_on_off: match data >> 1 {
                0 => Some(StartUpOnOffEnum::Off),
                1 => Some(StartUpOnOffEnum::On),
                2 => Some(StartUpOnOffEnum::Toggle),
                3 => None,
                _ => return Err(rs_matter::error::Error::from(ErrorCode::Failure)),
            },
        })
    }
}

pub struct OnOffDeviceLogic {
    start_up_on_off: Cell<Option<StartUpOnOffEnum>>,
    storage_path: PathBuf,
    max_brightness: u32,
}

const STORAGE_FILE_NAME: &str = "rs-matter-on-off-state";

impl OnOffDeviceLogic {
    pub fn new() -> Self {
        let max_brightness = get_max_backlight().unwrap_or(255);
        info!("Max Backlight Brightness Detected: {}", max_brightness);

        let storage_path = std::env::temp_dir().join(STORAGE_FILE_NAME);
        info!(
            "OnOffDeviceLogic using storage path: {}",
            storage_path.as_path().to_str().unwrap_or("none")
        );

        let persisted_state = match fs::File::open(storage_path.as_path()) {
            Ok(mut file) => {
                let mut buf: [u8; 1] = [0];
                file.read_exact(&mut buf).unwrap();
                OnOffPersistentState::from_bytes(buf[0]).unwrap()
            }
            Err(_) => OnOffPersistentState::default(),
        };

        Self {
            start_up_on_off: Cell::new(persisted_state.start_up_on_off),
            storage_path,
            max_brightness,
        }
    }

    // Helper to write brightness to the Linux file
    fn write_backlight(&self, brightness: u32) -> Result<(), Error> {
        let path = PathBuf::from(BACKLIGHT_DEVICE_PATH).join(BRIGHTNESS_FILE);
        fs::write(path, brightness.to_string().as_bytes()).map_err(|e| {
            error!("Failed to write to backlight file: {}", e);
            rs_matter::error::Error::from(ErrorCode::Failure)
        })
    }

    // Helper to read brightness from the Linux file
    fn read_backlight(&self) -> Result<u32, Error> {
        let path = PathBuf::from(BACKLIGHT_DEVICE_PATH).join(BRIGHTNESS_FILE);
        fs::read_to_string(path)
            .map_err(|e| {
                error!("Failed to read from backlight file: {}", e);
                rs_matter::error::Error::from(ErrorCode::Failure)
            })?
            .trim()
            .parse::<u32>()
            .map_err(|e| {
                error!("Failed to parse backlight value: {}", e);
                rs_matter::error::Error::from(ErrorCode::Failure)
            })
    }

    fn save_state(&self) -> Result<(), Error> {
        let mut file = fs::File::create(self.storage_path.as_path())?;

        // We persist 'false' for on_off because we read actual state from hardware
        let value = OnOffPersistentState::to_bytes_from_values(
            false, 
            self.start_up_on_off.get(),
        );

        file.write_all(&[value])?;
        Ok(())
    }
}

impl OnOffHooks for OnOffDeviceLogic {
    const CLUSTER: Cluster<'static> = on_off_cluster::FULL_CLUSTER
        .with_revision(6)
        .with_features(on_off_cluster::Feature::LIGHTING.bits())
        .with_attrs(with!(
            required;
            on_off_cluster::AttributeId::OnOff
            | on_off_cluster::AttributeId::GlobalSceneControl
            | on_off_cluster::AttributeId::OnTime
            | on_off_cluster::AttributeId::OffWaitTime
            | on_off_cluster::AttributeId::StartUpOnOff
        ))
        .with_cmds(with!(
            on_off_cluster::CommandId::Off
                | on_off_cluster::CommandId::On
                | on_off_cluster::CommandId::Toggle
                | on_off_cluster::CommandId::OffWithEffect
                | on_off_cluster::CommandId::OnWithRecallGlobalScene
                | on_off_cluster::CommandId::OnWithTimedOff
        ));

    fn on_off(&self) -> bool {
        match self.read_backlight() {
            Ok(brightness) => brightness > 0,
            Err(e) => {
                error!("Error reading backlight for OnOff state: {:?}", e);
                false
            }
        }
    }

    fn set_on_off(&self, on: bool) {
        info!("OnOff state commanded to: {}", on);
        
        // If ON, set to max_brightness. If OFF, set to 0.
        let target_brightness = if on { self.max_brightness } else { 0 };

        if let Err(err) = self.write_backlight(target_brightness) {
            error!("Error setting backlight for OnOff: {}", err);
        }

        if let Err(err) = self.save_state() {
            error!("Error saving state: {}", err);
        }
    }

    fn start_up_on_off(&self) -> Nullable<on_off::StartUpOnOffEnum> {
        match self.start_up_on_off.get() {
            Some(value) => Nullable::some(value),
            None => Nullable::none(),
        }
    }

    fn set_start_up_on_off(&self, value: Nullable<on_off::StartUpOnOffEnum>) -> Result<(), Error> {
        self.start_up_on_off.set(value.into_option());
        self.save_state()
    }

    async fn handle_off_with_effect(&self, _effect: on_off::EffectVariantEnum) {
        // no effect
    }
}
