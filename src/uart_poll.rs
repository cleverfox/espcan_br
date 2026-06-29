//! Async, full-duplex adapter over the *blocking* UART0 driver.
//!
//! On the classic ESP32, UART0 is the system console; driving it as an esp-hal
//! *async* UART under esp-rtos does not deliver RX, so we use the blocking driver
//! and poll it. This wraps both blocking halves in one object implementing
//! `embedded-io-async` Read + Write, so `transport::run_port` works unchanged.

use embassy_time::{Duration, Timer};
use embedded_io_async::{ErrorType, Read, Write};
use esp_hal::uart::{UartRx, UartTx};
use esp_hal::Blocking;

/// Poll interval when the RX FIFO is empty.
const POLL_MS: u64 = 1;

pub struct PolledUart {
    rx: UartRx<'static, Blocking>,
    tx: UartTx<'static, Blocking>,
}

impl PolledUart {
    pub fn new(rx: UartRx<'static, Blocking>, tx: UartTx<'static, Blocking>) -> Self {
        Self { rx, tx }
    }
}

impl ErrorType for PolledUart {
    type Error = core::convert::Infallible;
}

impl Read for PolledUart {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        loop {
            if let Ok(n) = self.rx.read_buffered(buf) {
                if n > 0 {
                    return Ok(n);
                }
            }
            Timer::after(Duration::from_millis(POLL_MS)).await;
        }
    }
}

impl Write for PolledUart {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        // Blocking FIFO write; SLCAN payloads are small, so this returns promptly.
        Ok(self.tx.write(buf).unwrap_or(0))
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        let _ = self.tx.flush();
        Ok(())
    }
}
