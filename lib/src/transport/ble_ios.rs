//! BLE transport implementation for iOS using Core Bluetooth

use core_bluetooth::{
    central::{
        characteristic::{Characteristic, WriteKind},
        peripheral::Peripheral,
        CentralEvent, CentralManager,
    },
    uuid::Uuid,
    ManagerState, Receiver,
};
use std::{fmt::Display, time::Duration};
use tracing::{debug, error, trace, warn};

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
    write_characteristic: Characteristic,
    notify_characteristic: Characteristic,
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

    async fn wait_for_power_on(&mut self) -> Result<(), Error> {
        while let Ok(event) = self.receiver.recv() {
            if let CentralEvent::ManagerStateChanged { new_state } = event {
                match new_state {
                    ManagerState::PoweredOn => return Ok(()),
                    ManagerState::Unsupported => return Err(Error::Unknown),
                    ManagerState::Unauthorized => return Err(Error::Unknown),
                    ManagerState::PoweredOff => continue,
                    _ => continue,
                }
            }
        }
        Err(Error::Unknown)
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

        Ok(BleDevice {
            info: info.clone(),
            peripheral: peripheral.clone(),
            write_characteristic: write_char,
            notify_characteristic: notify_char,
            mtu: 23, // Default MTU
        })
    }
}

const BLE_HEADER_LEN: usize = 3;

impl BleDevice {
    /// Helper to write commands as chunks based on device MTU
    async fn write_command(&mut self, cmd: u8, payload: &[u8]) -> Result<(), Error> {
        // Get write characteristic
        let write_characteristic = self
            .write_characteristic
            .as_ref()
            .ok_or_else(|| Error::Unknown)?;

        // Setup outgoing data (adds 2-byte big endian length prefix)
        let mut data = Vec::with_capacity(payload.len() + 2);
        data.extend_from_slice(&(payload.len() as u16).to_be_bytes()); // Data length
        data.extend_from_slice(payload); // Data

        debug!("TX cmd: 0x{cmd:02x} payload: {data:02x?}");

        // Write APDU in chunks
        for (i, c) in data.chunks(self.mtu as usize - BLE_HEADER_LEN).enumerate() {
            // Setup chunk buffer
            let mut buff = Vec::with_capacity(self.mtu as usize);
            let cmd = match i == 0 {
                true => cmd,
                false => 0x03,
            };

            buff.push(cmd); // Command
            buff.extend_from_slice(&(i as u16).to_be_bytes()); // Sequence ID
            buff.extend_from_slice(c);

            trace!("Write chunk {i}: {:02x?}", buff);

            self.peripheral.write_characteristic(
                write_characteristic,
                &buff,
                WriteKind::WithResponse,
            );

            // Wait for write completion
            while let Ok(event) = self.receiver.recv() {
                match event {
                    CentralEvent::WriteCharacteristicResult {
                        peripheral,
                        characteristic,
                        result,
                    } if peripheral.id() == self.peripheral.id()
                        && characteristic.id() == write_characteristic.id() =>
                    {
                        result.map_err(|_| Error::Unknown)?;
                        break;
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }

    /// Helper to read response packet from notification channel
    async fn read_data(&mut self) -> Result<Vec<u8>, Error> {
        // Get notify characteristic
        let notify_characteristic = self
            .notify_characteristic
            .as_ref()
            .ok_or_else(|| Error::Unknown)?;

        // Enable notifications
        self.peripheral.subscribe(notify_characteristic);

        // Wait for subscription result
        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::SubscriptionChangeResult {
                    peripheral,
                    characteristic,
                    result,
                } if peripheral.id() == self.peripheral.id()
                    && characteristic.id() == notify_characteristic.id() =>
                {
                    result.map_err(|_| Error::Unknown)?;
                    break;
                }
                _ => {}
            }
        }

        // Await first response
        let mut value = None;
        while let Ok(event) = self.receiver.recv() {
            match event {
                CentralEvent::CharacteristicValue {
                    peripheral,
                    characteristic,
                    value: val,
                } if peripheral.id() == self.peripheral.id()
                    && characteristic.id() == notify_characteristic.id() =>
                {
                    value = Some(val.map_err(|_| Error::Unknown)?);
                    break;
                }
                _ => {}
            }
        }

        let value = value.ok_or(Error::Closed)?;
        debug!("RX: {:02x?}", value);

        // Check response length is reasonable
        if value.len() < 5 {
            error!("response too short");
            return Err(Error::UnexpectedResponse);
        } else if value[0] != 0x05 {
            error!("unexpected response type: {:?}", value[0]);
            return Err(Error::UnexpectedResponse);
        }

        // Read out full response length
        let len = value[4] as usize;
        if len == 0 {
            return Err(Error::EmptyResponse);
        }

        trace!("Expecting response length: {}", len);

        // Setup response buffer
        let mut buff = Vec::with_capacity(len);
        buff.extend_from_slice(&value[5..]);

        // Read further responses
        while buff.len() < len {
            let mut value = None;
            while let Ok(event) = self.receiver.recv() {
                match event {
                    CentralEvent::CharacteristicValue {
                        peripheral,
                        characteristic,
                        value: val,
                    } if peripheral.id() == self.peripheral.id()
                        && characteristic.id() == notify_characteristic.id() =>
                    {
                        value = Some(val.map_err(|_| Error::Unknown)?);
                        break;
                    }
                    _ => {}
                }
            }

            let value = value.ok_or(Error::Closed)?;
            debug!("RX: {value:02x?}");

            // Add received data to buffer
            buff.extend_from_slice(&value[5..]);
        }

        // Disable notifications
        self.peripheral.unsubscribe(notify_characteristic);

        Ok(buff)
    }

    pub(crate) async fn is_connected(&self) -> Result<bool, Error> {
        // Core Bluetooth doesn't have a direct "is connected" API
        // We'll assume connected until we get a disconnect event
        Ok(true)
    }
}

#[cfg_attr(not(feature = "unstable_async_trait"), async_trait::async_trait)]
impl Exchange for BleDevice {
    async fn exchange(&mut self, command: &[u8], timeout: Duration) -> Result<Vec<u8>, Error> {
        // Write command data
        if let Err(e) = self.write_command(0x05, command).await {
            return Err(e);
        }

        debug!("Await response");

        // Wait for response with timeout
        match tokio::time::timeout(timeout, self.read_data()).await {
            Ok(Ok(buff)) => Ok(buff),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        }
    }
}
