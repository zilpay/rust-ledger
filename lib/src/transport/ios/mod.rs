#[cfg(target_os = "ios")]
mod ble;
#[cfg(target_os = "ios")]
mod usb;

#[cfg(target_os = "ios")]
pub use ble::{BleDevice as IosBleDevice, BleInfo as IosBleInfo, BleTransport as IosBleTransport};
#[cfg(target_os = "ios")]
pub use usb::{UsbDevice as IosUsbDevice, UsbInfo as IosUsbInfo, UsbTransport as IosUsbTransport};
