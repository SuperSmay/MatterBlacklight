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

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::env;


use std::sync::{LazyLock, Mutex, Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;

use embassy_futures::select::{select3, select4};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;

use async_signal::{Signal, Signals};
use log::{error, info, debug}; 

use futures_lite::StreamExt;

use dirs;

use rs_matter::dm::clusters::decl::level_control::{
    AttributeId, CommandId, OptionsBitmap, FULL_CLUSTER as LEVEL_CONTROL_FULL_CLUSTER,
};

use rs_matter::dm::clusters::decl::on_off as on_off_cluster;
use rs_matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter::dm::clusters::net_comm::NetworkType;
use rs_matter::dm::clusters::level_control::{self, LevelControlHooks};
use rs_matter::dm::clusters::on_off::{self, OnOffHooks, StartUpOnOffEnum};
use rs_matter::dm::devices::test::{TEST_DEV_ATT, TEST_DEV_COMM, TEST_DEV_DET};
use rs_matter::dm::devices::DEV_TYPE_DIMMABLE_LIGHT;
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
// `rs-matter` supports efficient initialization of BSS objects (with `init`)
// as well as just allocating the objects on-stack or on the heap.
static MATTER: StaticCell<Matter> = StaticCell::new();
static BUFFERS: StaticCell<PooledBuffers<10, NoopRawMutex, IMBuffer>> = StaticCell::new();
static SUBSCRIPTIONS: StaticCell<DefaultSubscriptions> = StaticCell::new();
static PSM: StaticCell<Psm<4096>> = StaticCell::new();

fn main() -> Result<(), Error> {

    let args: Vec<String> = env::args().collect();
    let parsed_args = parse_args(&args);

    // Initialize logging
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );

    let filesystem_manager = Arc::new(FilesystemManager { no_filesystem: parsed_args.no_filesystem });

    // I don't know enough about threads and Rust to fully get this, but Copilot seems to think it works
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    let auto_brightness_filesystem_manager = filesystem_manager.clone();
    let auto_brightness_thread = std::thread::Builder::new()
        .name("auto-brightness".into())
        .spawn(move || {
            while r.load(Ordering::SeqCst) {
                auto_brightness_thread_run(auto_brightness_filesystem_manager.clone());
            }
        })
        .unwrap();

    if parsed_args.no_filesystem {
        info!("Running in no-filesystem mode. No actual filesystem changes will be made, and system files will not be accessed.");
        if parsed_args.no_als_spoof {
            info!("Ambient light sensor spoofing disabled.");
        } else {
            info!("Ambient light sensor spoofing enabled.");
        }
    }

    if parsed_args.no_filesystem && !parsed_args.no_als_spoof {
        info!("Starting ambient light sensor spoofing thread.");
        let _als_spoof_thread = std::thread::Builder::new()
            .spawn(move || {
            let mut increasing = true;
                loop {
                    if increasing {
                        let mut als_value = AMBIENT_LIGHT_VALUE_DRY.lock().unwrap();
                        *als_value += 5;
                        if *als_value >= AMBIENT_LIGHT_SENSOR_MAX {
                            increasing = false;
                        }
                    } else {
                        let mut als_value = AMBIENT_LIGHT_VALUE_DRY.lock().unwrap();
                        *als_value -= 5;
                        if *als_value <= 0 {
                            increasing = true;
                        }
                    }
                    // Sleep for a while
                    std::thread::sleep(Duration::from_secs(10));
                }
            })
            .unwrap();
    }

    let matter_filesystem_manager = filesystem_manager.clone();

    let thread = std::thread::Builder::new()
        // Increase the stack size until the example can work without stack blowups.
        .stack_size(550 * 1024)
        .spawn(move || run(matter_filesystem_manager.clone()))
        .unwrap();

    // Wait on Matter thread, then if it crashes, just run the auto brightness thread forever
    match thread.join() {
        Ok(_) => running.store(false, Ordering::SeqCst),
        Err(e) => {
            error!("Matter thread exited with error: {:?}", e);
        }
    }

    match auto_brightness_thread.join() {
        Ok(_) => Ok(()),
        Err(e) => {
            error!("Auto brightness thread exited with error: {:?}", e);
            Err(Error::from(ErrorCode::Failure))
        }
    }

}

struct Args {
    no_filesystem: bool,
    no_als_spoof: bool
}

fn parse_args(args: &[String]) -> Args {
    Args {
        no_filesystem: args.contains(&String::from("--no-filesystem")),
        no_als_spoof: args.contains(&String::from("--no-als-spoof")),
    }
}
fn run(filesystem_manager: Arc<FilesystemManager>) -> Result<(), Error> {


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

    // Need to call this once
    matter.initialize_transport_buffers()?;

    // Create the transport buffers
    let buffers = BUFFERS.uninit().init_with(PooledBuffers::init(0));

    // Create the subscriptions
    let subscriptions = SUBSCRIPTIONS
        .uninit()
        .init_with(DefaultSubscriptions::init());

    // LevelControl cluster setup
    let level_control_handler = level_control::LevelControlHandler::new(
        Dataver::new_rand(matter.rand()),
        1,
        LevelControlDeviceLogic::new(filesystem_manager.clone()),
        level_control::AttributeDefaults {
            on_level: Nullable::some(42),
            options: OptionsBitmap::from_bits(OptionsBitmap::EXECUTE_IF_OFF.bits()).unwrap(),
            ..Default::default()
        },
    );

    // OnOff cluster setup
    let on_off_handler =
        on_off::OnOffHandler::new(Dataver::new_rand(matter.rand()), 1, OnOffDeviceLogic::new(filesystem_manager.clone()));

    // Cluster wiring, validation and initialisation
    on_off_handler.init(Some(&level_control_handler));
    level_control_handler.init(Some(&on_off_handler));

    // Create the Data Model instance
    let dm = DataModel::new(
        matter,
        buffers,
        subscriptions,
        dm_handler(matter, &on_off_handler, &level_control_handler),
    );

    // Create a default responder capable of handling up to 3 subscriptions
    // All other subscription requests will be turned down with "resource exhausted"
    let responder = DefaultResponder::new(&dm);
    info!(
        "Responder memory: Responder (stack)={}B, Runner fut (stack)={}B",
        core::mem::size_of_val(&responder),
        core::mem::size_of_val(&responder.run::<4, 4>())
    );

    // Run the responder with up to 4 handlers (i.e. 4 exchanges can be handled simultaneously)
    // Clients trying to open more exchanges than the ones currently running will get "I'm busy, please try again later"
    let mut respond = pin!(responder.run::<4, 4>());
    
    // Run the background job of the data model
    let mut dm_job = pin!(dm.run());

    // Create a dual-stack UDP socket for Matter (accepts IPv4 and IPv6)
    let matter_sock = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    matter_sock.set_reuse_address(true)?;
    matter_sock.set_only_v6(false)?;
    matter_sock.bind(&rs_matter::transport::MATTER_SOCKET_BIND_ADDR.into())?;
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(matter_sock.into())?;

    // Helpful debug logging: print the bound local address and some env vars so we can
    // diagnose differences when running as root vs a normal user.
    match socket.get_ref().local_addr() {
        Ok(addr) => info!("Matter socket local addr: {}", addr),
        Err(e) => error!("Failed to read matter socket local_addr(): {}", e),
    }

    info!(
        "User env: USER='{}' SUDO_USER='{}' (process id={})",
        env::var("USER").unwrap_or_else(|_| "<unset>".into()),
        env::var("SUDO_USER").unwrap_or_else(|_| "<unset>".into()),
        std::process::id()
    );

    info!(
        "Transport memory: Transport fut (stack)={}B, mDNS fut (stack)={}B",
        core::mem::size_of_val(&matter.run(&socket, &socket)),
        core::mem::size_of_val(&mdns::run_mdns(matter))
    );

    // Run the Matter and mDNS transports
    let mut mdns = pin!(mdns::run_mdns(matter));
    let mut transport = pin!(matter.run(&socket, &socket));

    // Create, load and run the persister
    let psm = PSM.uninit().init_with(Psm::init());
    let path = dirs::config_dir().unwrap_or(std::env::temp_dir()).join("rs-matter");

    info!(
        "Persist memory: Persist (BSS)={}B, Persist fut (stack)={}B, Persist path={}",
        core::mem::size_of::<Psm<4096>>(),
        core::mem::size_of_val(&psm.run(&path, matter, NO_NETWORKS)),
        path.as_path().to_str().unwrap_or("none")
    );

    psm.load(&path, matter, NO_NETWORKS)?;

    matter.print_standard_qr_text(DiscoveryCapabilities::IP)?;

    if !matter.is_commissioned() {
        // If the device is not commissioned yet, print the QR code to the console
        // and enable basic commissioning
        matter.print_standard_qr_code(QrTextType::Unicode, DiscoveryCapabilities::IP)?;
        matter.open_basic_comm_window(MAX_COMM_WINDOW_TIMEOUT_SECS)?;
    }

    let mut persist = pin!(psm.run(&path, matter, NO_NETWORKS));

    // Listen to SIGTERM because at the end of the test we'll receive it
    let mut term_signal = Signals::new([Signal::Term])?;
    let mut term = pin!(async {
        term_signal.next().await;
        Ok(())
    });

    // Combine all async tasks in a single one
    let all = select4(
        &mut transport,
        &mut mdns,
        &mut persist,
        select3(&mut respond, &mut dm_job, &mut term).coalesce(),
    );

    // Run with a simple `block_on`. Any local executor would do.
    futures_lite::future::block_on(all.coalesce())
}

/// The Node meta-data describing our Matter device.
const NODE: Node<'static> = Node {
    id: 0,
    endpoints: &[
        endpoints::root_endpoint(NetworkType::Ethernet),
        Endpoint {
            id: 1,
            device_types: devices!(DEV_TYPE_DIMMABLE_LIGHT),
            clusters: clusters!(
                desc::DescHandler::CLUSTER,
                OnOffDeviceLogic::CLUSTER,
                LevelControlDeviceLogic::CLUSTER,
            ),
        },
    ],
};

/// The Data Model handler + meta-data for our Matter device.
/// The handler is the root endpoint 0 handler plus the on-off handler and its descriptor.
fn dm_handler<'a, LH: LevelControlHooks, OH: OnOffHooks>(
    matter: &'a Matter<'a>,
    on_off: &'a on_off::OnOffHandler<'a, OH, LH>,
    level_control: &'a level_control::LevelControlHandler<'a, LH, OH>,
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
                    ).chain(
                        EpClMatcher::new(Some(1), Some(LevelControlDeviceLogic::CLUSTER.id)),
                        level_control::HandlerAsyncAdaptor(level_control),
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
const AMBIENT_LIGHT_SENSOR_PATH: &str = "/sys/bus/iio/devices/iio:device0/in_illuminance_raw";

// Max raw sensor value you expect (e.g., test in bright sunlight)
const AMBIENT_LIGHT_SENSOR_MAX: u32 = 70;
// Minimum brightness (1 is usually the lowest, not 0)
const MIN_BRIGHTNESS: u32 = 5;
// How aggressively to scale (0.1 - 1.0). Higher = brighter faster.
// const SCALE_FACTOR: f32 = 0.7;

static BACKLIGHT_VALUE_DRY: LazyLock<Mutex<u32>> = LazyLock::new(|| {
    Mutex::new(100)
});

const BACKLIGHT_MAX_VALUE_DRY: u32 = 1000;

static AMBIENT_LIGHT_VALUE_DRY: LazyLock<Mutex<u32>> = LazyLock::new(|| {
    Mutex::new(50)
});

static AMBIENT_LIGHT_RATIO: LazyLock<Mutex<f32>> = LazyLock::new(|| {
    Mutex::new(1.0)
});

static MATTER_BRIGHTNESS_RATIO: LazyLock<Mutex<f32>> = LazyLock::new(|| {
    Mutex::new(1.0)
});

static MATTER_PREV_BRIGHTNESS_RATIO: LazyLock<Mutex<f32>> = LazyLock::new(|| {
    Mutex::new(1.0)
});


// --- END: Backlight Control Configuration ---


// Implementing the LevelControl business logic
pub struct LevelControlDeviceLogic {
    start_up_current_level: Cell<Option<u8>>,
    max_brightness: u32,
    filesystem_manager: Arc<FilesystemManager>,
}

impl Default for LevelControlDeviceLogic {
    fn default() -> Self {
        Self::new(Arc::new(FilesystemManager { no_filesystem: false }))
    }
}

impl LevelControlDeviceLogic {
    pub fn new(filesystem_manager: Arc<FilesystemManager>) -> Self {
        let max_brightness = filesystem_manager.get_max_backlight().unwrap_or(255); // Fallback to 255
        info!("Max Backlight Brightness Detected: {}", max_brightness);

        Self {
            start_up_current_level: Cell::new(None),
            filesystem_manager,
            max_brightness,
        }
    }
}

impl LevelControlHooks for LevelControlDeviceLogic {
    const MIN_LEVEL: u8 = 1;
    const MAX_LEVEL: u8 = 254;
    const FASTEST_RATE: u8 = 50;
    const CLUSTER: Cluster<'static> = LEVEL_CONTROL_FULL_CLUSTER
        .with_features(
            level_control::Feature::LIGHTING.bits() | level_control::Feature::ON_OFF.bits(),
        )
        .with_attrs(with!(
            required;
            AttributeId::CurrentLevel
            | AttributeId::RemainingTime
            | AttributeId::MinLevel
            | AttributeId::MaxLevel
            | AttributeId::OnOffTransitionTime
            | AttributeId::OnLevel
            | AttributeId::OnTransitionTime
            | AttributeId::OffTransitionTime
            | AttributeId::DefaultMoveRate
            | AttributeId::Options
            | AttributeId::StartUpCurrentLevel
        ))
        .with_cmds(with!(
            CommandId::MoveToLevel
                | CommandId::Move
                | CommandId::Step
                | CommandId::Stop
                | CommandId::MoveToLevelWithOnOff
                | CommandId::MoveWithOnOff
                | CommandId::StepWithOnOff
                | CommandId::StopWithOnOff
        ));

    fn set_device_level(&self, level: u8) -> Result<Option<u8>, ()> {
        // Actually do the thing

        // Get the auto brightness level
        let ambient_light_ratio = match AMBIENT_LIGHT_RATIO.lock() {
            Ok(value) => *value,
            Err(_) => 1.0
        };
        let matter_ratio = level as f32/255.0;
        // Store the ratio for the auto brightness part to use
        match MATTER_BRIGHTNESS_RATIO.lock() {
            Ok(mut value) => *value = matter_ratio,
            Err(_) => {error!("Could not set Matter brightness ratio variable")}
        };


        let scaled_brightness_result = self.filesystem_manager.map_matter_and_ambient_light_to_brightness(matter_ratio, ambient_light_ratio);
        if let Err(err) = scaled_brightness_result {
            error!("Could not calculate scaled brightness");
            return Err(());
        }

        let scaled_brightness = scaled_brightness_result.unwrap();

        let write_result = self.filesystem_manager.write_backlight(scaled_brightness as u32);

        if let Err(err) = write_result {
            error!("Failed to write backlight brightness: {}", err);
            return Err(());
        }

        info!("LevelControl: Matter Level {level} and ambient light ratio {ambient_light_ratio} mapped to Backlight Brightness {scaled_brightness}");
        Ok(Some(level))
    }

    fn current_level(&self) -> Option<u8> {
        match MATTER_BRIGHTNESS_RATIO.lock() {
            Ok(brightness) => {
                Some((*brightness * 255.0) as u8)
            },
            Err(e) => {
                error!("Error reading current backlight brightness: {:?}", e);
                None
            }
        }
    }

    // set_current_level is now a no-op as state is read from the device
    fn set_current_level(&self, _level: Option<u8>) {
        // State is now managed by reading the backlight file in `current_level`
    }

    fn start_up_current_level(&self) -> Result<Option<u8>, Error> {
        Ok(self.start_up_current_level.get())
    }

    fn set_start_up_current_level(&self, value: Option<u8>) -> Result<(), Error> {
        self.start_up_current_level.set(value);
        Ok(())
    }
}

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
    filesystem_manager: Arc<FilesystemManager>,
}

const STORAGE_FILE_NAME: &str = "rs-matter-on-off-state";

impl OnOffDeviceLogic {
    pub fn new(filesystem_manager: Arc<FilesystemManager>) -> Self {
        let max_brightness = filesystem_manager.get_max_backlight().unwrap_or(255);
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
            filesystem_manager,
        }
    }

    

    fn save_state(&self) -> Result<(), Error> {
        let mut file = fs::File::create(self.storage_path.as_path())?;

        // Use a dummy 'on_off' value (false) as the real on/off is derived from backlight level.
        // We only persist the `start_up_on_off`.
        let value = OnOffPersistentState::to_bytes_from_values(
            false, 
            self.start_up_on_off.get(),
        );

        let buf = &[value];

        file.write_all(buf)?;

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
        match self.filesystem_manager.read_backlight() {
            Ok(brightness) => brightness > 0,
            Err(e) => {
                error!("Error reading backlight for OnOff state: {:?}", e);
                false
            }
        }
    }

    fn set_on_off(&self, on: bool) {
        info!("OnOff state commanded to: {}", on);
        
        // Get the auto brightness level
        let ambient_light_ratio = match AMBIENT_LIGHT_RATIO.lock() {
            Ok(value) => *value,
            Err(_) => 1.0
        };
        let prev_matter_ratio = match MATTER_PREV_BRIGHTNESS_RATIO.lock() {
            Ok(value) => *value,
            Err(_) => {1.0}
        };

        let target_brightness = if on { 
            match self.filesystem_manager.map_matter_and_ambient_light_to_brightness(prev_matter_ratio, ambient_light_ratio) {
                Ok(value) => value as u32,
                Err(err) => {
                    error!("Error calculating target brightness for OnOff: {}", err);
                    self.max_brightness
                }
            }
        } else { 
            match MATTER_PREV_BRIGHTNESS_RATIO.lock() {
                Ok(mut value) => {
                    let matter_ratio = match MATTER_BRIGHTNESS_RATIO.lock() {
                        Ok(value) => *value,
                        Err(_) => {1.0}
                    };
                    *value = matter_ratio;
                },
                Err(_) => {}
            };
            0 
        };

        info!("Backlight value is: {target_brightness}");

        if let Err(err) = self.filesystem_manager.write_backlight(target_brightness) {
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

// Filesystem helpers
struct FilesystemManager {
    no_filesystem: bool,
}

impl FilesystemManager {
    // Helper to read max brightness
    fn get_max_backlight(&self) -> Result<u32, Error> {

        if self.no_filesystem {
            return Ok(BACKLIGHT_MAX_VALUE_DRY);
        }

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

    // Helper to write brightness to the Linux file
    fn write_backlight(&self, brightness: u32) -> Result<(), Error> {

        if self.no_filesystem {
            *BACKLIGHT_VALUE_DRY.lock().unwrap() = brightness;
            return Ok(());
        }

        let path = PathBuf::from(BACKLIGHT_DEVICE_PATH).join(BRIGHTNESS_FILE);
        fs::write(path, brightness.to_string().as_bytes()).map_err(|e| {
            error!("Failed to write to backlight file: {}", e);
            rs_matter::error::Error::from(ErrorCode::Failure)
        })
    }


    // Helper to read brightness from the Linux file
    fn read_backlight(&self) -> Result<u32, Error> {

        if self.no_filesystem {
            let value = *BACKLIGHT_VALUE_DRY.lock().unwrap();
            return Ok(value);
        }

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

    // Helper to read ambient light sensor value from the Linux file
    fn read_ambient_light_sensor(&self) -> Result<u32, Error> {

        if self.no_filesystem {
            let value = *AMBIENT_LIGHT_VALUE_DRY.lock().unwrap();
            return Ok(value);
        }

        let path = PathBuf::from(AMBIENT_LIGHT_SENSOR_PATH);
        fs::read_to_string(path)
            .map_err(|e| {
                error!("Failed to read from ambient light sensor file: {}", e);
                rs_matter::error::Error::from(ErrorCode::Failure)
            })?
            .trim()
            .parse::<u32>()
            .map_err(|e| {
                error!("Failed to parse ambient light sensor value: {}", e);
                rs_matter::error::Error::from(ErrorCode::Failure)
            })
    }

    fn map_matter_and_ambient_light_to_brightness(&self, matter_ratio: f32, ambient_light_ratio: f32) -> Result<f32, Error> {

        if matter_ratio <= 0.0 {
            return Ok(0.0);
        }

        let backlight_max_value = self.get_max_backlight()?;
        let brightness_range = backlight_max_value - MIN_BRIGHTNESS;

        // Scale sensor value to brightness
        let mut scaled_brightness = (MIN_BRIGHTNESS as f32 + (matter_ratio * ambient_light_ratio * (brightness_range as f32))) as f32;

        // Clamp to min and max
        if scaled_brightness < MIN_BRIGHTNESS as f32 {
            scaled_brightness = MIN_BRIGHTNESS as f32;
        } else if scaled_brightness > backlight_max_value as f32 {
            scaled_brightness = backlight_max_value as f32;
        }

        Ok(scaled_brightness)
    }
}


// Ambient Light Sensor Stuff
// Blocking loop that does all the work for auto brightness
fn auto_brightness_thread_run(filesystem_manager: Arc<FilesystemManager>) {



    let mut prev_smoothed_value = filesystem_manager.read_ambient_light_sensor().unwrap_or(0) as f32;
    
    fn loop_runner(prev_smoothed_value: &mut f32, filesystem_manager: Arc<FilesystemManager>) -> Result<(), Error> {

        let alpha = 0.2;

        let sensor_value = filesystem_manager.read_ambient_light_sensor()?;
        // Apply EWMA to sensor value
        let smoothed_sensor_value = alpha * (sensor_value as f32) + (1.0 - alpha) * *prev_smoothed_value;
        *prev_smoothed_value = smoothed_sensor_value;
        
        // Max the value at 1.0
        let ambient_light_ratio = 1.0_f32.min(smoothed_sensor_value as f32 / AMBIENT_LIGHT_SENSOR_MAX as f32);
        // Store the ratio for the matter side to use
        match AMBIENT_LIGHT_RATIO.lock() {
            Ok(mut value) => *value = ambient_light_ratio,
            Err(_) => {error!("Could not set ambient light ratio variable")}
        };

        // Retrieve the matter ratio or default to 1.0 if we can't
        let matter_brightness = match MATTER_BRIGHTNESS_RATIO.lock() {
            Ok(value) => *value,
            Err(_) => 1.0
        };

        let scaled_brightness = filesystem_manager.map_matter_and_ambient_light_to_brightness(matter_brightness, ambient_light_ratio)?;

        debug!(
            "Ambient light sensor: {sensor_value}, smoothed, light sensor value: {smoothed_sensor_value}, setting brightness to: {scaled_brightness}"
        );


        if let Err(err) = filesystem_manager.write_backlight(scaled_brightness as u32) {
            error!("Error setting backlight for auto brightness: {}", err);
        }

        Ok(())
    }

    loop {
        match loop_runner(&mut prev_smoothed_value, filesystem_manager.clone()) {
            Ok(()) => {
                ()
            }
            Err(e) => {
                error!("Error reading ambient light sensor: {:?}", e);
            }
        }

        // Sleep for a while before next reading
        std::thread::sleep(Duration::from_secs(1));
    }
}