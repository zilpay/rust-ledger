//! BLE transport implementation for iOS using Core Bluetooth

use core_bluetooth::{
    central::{
        characteristic::{Characteristic, WriteKind},
        peripheral::Peripheral,
        CentralEvent, CentralManager,
    },
    uuid::Uuid,
    Receiver,
};
use std::{fmt::Display, time::Duration};
use tracing::{debug, warn};

use super::{Exchange, Transport};
use crate::{
    info::{LedgerInfo, Model},
    Error,
};

/// Transport for listing and connecting to BLE connected Ledger devices
pub struct BleTransport {
    central: CentralManager,
    receiver: Receiver<CentralEvent>,
    peripherals: Vec<(LedgerInfo, Peripheral)>,
}

/// BLE specific device information
#[derive(Clone, PartialEq, Debug)]
pub struct BleInfo {
    name: String,
    addr: Uuid,
}

impl Display for BleInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// BLE connected ledger device
pub struct BleDevice {
    pub info: BleInfo,
    mtu: u8,
    peripheral: Peripheral,
    characteristic_write: Characteristic,
    characteristic_read: Characteristic,
    receiver: Receiver<CentralEvent>,
}

/// Bluetooth spec for ledger devices
#[derive(Clone, PartialEq, Debug)]
struct BleSpec {
    pub model: Model,
    pub service_uuid: uuid::Uuid,
    pub notify_uuid: uuid::Uuid,
    pub write_uuid: uuid::Uuid,
    pub write_cmd_uuid: uuid::Uuid,
}

/// Spec for types of bluetooth device
const BLE_SPECS: &[BleSpec] = &[
    BleSpec {
        model: Model::NanoX,
        service_uuid: uuid::uuid!("13d63400-2c97-0004-0000-4c6564676572"),
        notify_uuid: uuid::uuid!("13d63400-2c97-0004-0001-4c6564676572"),
        write_uuid: uuid::uuid!("13d63400-2c97-0004-0002-4c6564676572"),
        write_cmd_uuid: uuid::uuid!("13d63400-2c97-0004-0003-4c6564676572"),
    },
    BleSpec {
        model: Model::Stax,
        service_uuid: uuid::uuid!("13d63400-2c97-6004-0000-4c6564676572"),
        notify_uuid: uuid::uuid!("13d63400-2c97-6004-0001-4c6564676572"),
        write_uuid: uuid::uuid!("13d63400-2c97-6004-0002-4c6564676572"),
        write_cmd_uuid: uuid::uuid!("13d63400-2c97-6004-0003-4c6564676572"),
    },
];

impl BleTransport {
    pub async fn new() -> Result<Self, Error> {
        // Setup Core Bluetooth central manager
        let (central, receiver) = CentralManager::new();

        // Return transport instance
        Ok(Self {
            central,
            receiver,
            peripherals: vec![],
        })
    }

    /// Helper to scan for available BLE devices
    async fn scan_internal(
        &mut self,
        duration: Duration,
    ) -> Result<Vec<(LedgerInfo, Peripheral)>, Error> {
        let mut matched = vec![];

        // Start scanning with empty options
        self.central.scan();

        // Wait for duration
        tokio::time::sleep(duration).await;

        // Process discovered devices from receiver
        while let Ok(event) = self.receiver.try_recv() {
            if let CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                ..
            } = event
            {
                // Get device name
                let uuid = peripheral.id().to_string();
                let name = advertisement_data.local_name().unwrap_or(&uuid);

                // Match on peripheral names
                let model = if name.contains("Nano X") {
                    Model::NanoX
                } else if name.contains("Stax") {
                    Model::Stax
                } else {
                    continue;
                };

                // Add to device list
                matched.push((
                    LedgerInfo {
                        model: model.clone(),
                        conn: BleInfo {
                            name: name.to_string(),
                            addr: peripheral.id(),
                        }
                        .into(),
                    },
                    peripheral,
                ));
            }
        }

        Ok(matched)
    }
}

#[cfg_attr(not(feature = "unstable_async_trait"), async_trait::async_trait)]
impl Transport for BleTransport {
    type Filters = ();
    type Info = BleInfo;
    type Device = BleDevice;

    /// List BLE connected ledger devices
    async fn list(&mut self, _filters: Self::Filters) -> Result<Vec<LedgerInfo>, Error> {
        // Scan for available devices
        let devices = self.scan_internal(Duration::from_millis(1000)).await?;

        // Filter to return info list
        let info: Vec<_> = devices.iter().map(|d| d.0.clone()).collect();

        // Save listed devices for next connect
        self.peripherals = devices;

        Ok(info)
    }

    /// Connect to a specific ledger device
    async fn connect(&mut self, info: Self::Info) -> Result<Self::Device, Error> {
        // Match known peripherals using provided device info
        let (d, p) = match self
            .peripherals
            .iter()
            .find(|(d, _p)| d.conn == info.clone().into())
        {
            Some(v) => v,
            None => {
                warn!("No device found matching: {info:?}");
                return Err(Error::NoDevices);
            }
        };

        let peripheral = p.clone();

        // Fetch specs for matched model
        let specs = match BLE_SPECS.iter().find(|s| s.model == d.model) {
            Some(v) => v,
            None => {
                warn!("No specs for model: {:?}", d.model);
                return Err(Error::Unknown);
            }
        };

        // Connect to device
        self.central.connect(&peripheral);

        // Wait for connection
        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::PeripheralConnected { peripheral: p, .. }
                    if p.id() == peripheral.id() =>
                {
                    break;
                }
                CentralEvent::PeripheralConnectFailed { peripheral: p, .. }
                    if p.id() == peripheral.id() =>
                {
                    return Err(Error::Unknown);
                }
                _ => continue,
            }
        }

        debug!("Connected to peripheral");

        // Discover services
        let uuid = Uuid::from_bytes(*specs.service_uuid.as_bytes());
        peripheral.discover_services_with_uuids(&[uuid]);

        // Wait for services discovery
        let mut service = None;
        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::ServicesDiscovered {
                    peripheral: p,
                    services,
                    ..
                } if p.id() == peripheral.id() => match services {
                    Ok(services) => {
                        service = services.into_iter().next();
                        break;
                    }
                    Err(_) => return Err(Error::Unknown),
                },
                _ => continue,
            }
        }

        let service = service.ok_or(Error::Unknown)?;

        let notify_uuid = Uuid::from_bytes(*specs.notify_uuid.as_bytes());
        let write_uuid = Uuid::from_bytes(*specs.write_uuid.as_bytes());

        peripheral.discover_characteristics_with_uuids(&service, &[notify_uuid, write_uuid]);

        let mut notify_char = None;
        let mut write_char = None;

        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::CharacteristicsDiscovered {
                    peripheral: p,
                    characteristics,
                    ..
                } if p.id() == peripheral.id() => match characteristics {
                    Ok(chars) => {
                        for char in chars {
                            if char.id() == notify_uuid {
                                notify_char = Some(char);
                            } else if char.id() == write_uuid {
                                write_char = Some(char);
                            }
                        }
                        break;
                    }
                    Err(_) => return Err(Error::Unknown),
                },
                _ => continue,
            }
        }

        let notify_char = notify_char.ok_or(Error::Unknown)?;
        let write_char = write_char.ok_or(Error::Unknown)?;

        // Subscribe to notifications
        peripheral.subscribe(&notify_char);

        // Wait for successful subscription
        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::SubscriptionChangeResult {
                    peripheral: p,
                    result,
                    ..
                } if p.id() == peripheral.id() => match result {
                    Ok(_) => break,
                    Err(_) => return Err(Error::Unknown),
                },
                _ => continue,
            }
        }

        // Create a new CentralManager for the device
        let (_central, receiver) = CentralManager::new();

        // Create device instance
        let device = BleDevice {
            info: info.clone(),
            mtu: 23,
            peripheral: peripheral.clone(),
            characteristic_write: write_char,
            characteristic_read: notify_char,
            receiver,
        };

        Ok(device)
    }
}

pub const BLE_HEADER_LEN: usize = 3;

impl BleDevice {
    /// Helper to write commands as chunks based on device MTU
    pub async fn write_command(&mut self, cmd: u8, payload: &[u8]) -> Result<(), Error> {
        let mut data = Vec::with_capacity(payload.len() + 2);
        data.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        data.extend_from_slice(payload);

        debug!("TX cmd: 0x{cmd:02x} payload: {data:02x?}");

        for (i, chunk) in data.chunks(self.mtu as usize - BLE_HEADER_LEN).enumerate() {
            let mut buff = Vec::with_capacity(self.mtu as usize);
            let cmd = if i == 0 { cmd } else { 0x03 };

            buff.push(cmd);
            buff.extend_from_slice(&(i as u16).to_be_bytes());
            buff.extend_from_slice(chunk);

            debug!("Write chunk {i}: {:02x?}", buff);

            self.peripheral.write_characteristic(
                &self.characteristic_write,
                &buff,
                WriteKind::WithResponse,
            );
        }

        Ok(())
    }

    pub(crate) async fn is_connected(&self) -> Result<bool, Error> {
        // Core Bluetooth doesn't provide direct way to check connection status
        // Assume connected if we have valid device
        Ok(true)
    }
}

#[cfg_attr(not(feature = "unstable_async_trait"), async_trait::async_trait)]
impl Exchange for BleDevice {
    async fn exchange(&mut self, command: &[u8], timeout: Duration) -> Result<Vec<u8>, Error> {
        // Subscribe to notifications
        self.peripheral.subscribe(&self.characteristic_read);

        // Write command
        let mut data = Vec::with_capacity(command.len() + 2);
        data.extend_from_slice(&(command.len() as u16).to_be_bytes());
        data.extend_from_slice(command);

        self.peripheral.write_characteristic(
            &self.characteristic_write,
            &data,
            core_bluetooth::central::characteristic::WriteKind::WithResponse,
        );

        // Wait for response through events
        let start = std::time::Instant::now();
        let mut response = None;

        // Trigger characteristic read
        self.peripheral
            .read_characteristic(&self.characteristic_read);

        // Process events through central manager
        while response.is_none() && start.elapsed() < timeout {
            if let Ok(event) = self.receiver.recv() {
                if let CentralEvent::CharacteristicValue {
                    characteristic,
                    value,
                    ..
                } = event
                {
                    if characteristic.id() == self.characteristic_read.id() {
                        if let Ok(data) = value {
                            response = Some(data);
                        }
                    }
                }
            }
        }

        // Unsubscribe
        self.peripheral.unsubscribe(&self.characteristic_read);

        match response {
            Some(data) => Ok(data),
            None => Err(Error::Timeout),
        }
    }
}
