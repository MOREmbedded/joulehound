//! The JS220 publish/subscribe topic tree.
//!
//! Topics published over port 1 are relative to the device; the host-side
//! driver prefixes them with `{backend}/{model}/{serial_number}` when it
//! re-publishes them to applications.
//!
//! Two branches matter for streaming:
//!
//! - `c/...` targets the USB controller microcontroller.
//! - `s/...` targets the sensor FPGA.
//!
//! A leading `!` on the final element marks a command, which is never retained.

/// Controller firmware version, `u32` packed as major8.minor8.patch16.
pub const C_FW_VERSION: &str = "c/fw/version";
/// PCB version, `u32` packed as major8.minor8.patch16.
pub const C_HW_VERSION: &str = "c/hw/version";
/// Controller mode: bootloader or application.
pub const C_MODE: &str = "c/s/mode";
/// Sensor power: 0 off, 1 on.
pub const C_POWER: &str = "c/s/power";
/// Controller-to-sensor link: 0 disconnected, 1 connected.
pub const C_COMM: &str = "c/s/comm";
/// Reference voltage for the general purpose IO: `external` or `3v3`.
pub const C_GPIO_VREF: &str = "c/gpio/vref";

/// Sensor FPGA version, `u32` packed as major8.minor8.patch16.
pub const S_FPGA_VERSION: &str = "s/fpga/version";
/// Output sample rate shared by current, voltage and power, in Hz.
pub const S_SAMPLE_RATE: &str = "s/fs";

/// Global enable for every `!data` stream.
pub const S_STREAM_CTRL: &str = "s/stream/ctrl";

/// Enable the current stream.
pub const S_CURRENT_CTRL: &str = "s/i/ctrl";
/// Enable the voltage stream.
pub const S_VOLTAGE_CTRL: &str = "s/v/ctrl";
/// Enable the power stream.
pub const S_POWER_CTRL: &str = "s/p/ctrl";

/// Enable the 4-bit current range stream.
pub const S_CURRENT_RANGE_CTRL: &str = "s/i/range/ctrl";
/// Current range selection mode: `auto`, `manual` or `off`.
pub const S_CURRENT_RANGE_MODE: &str = "s/i/range/mode";
/// Current range to use in manual mode, 0..=5.
pub const S_CURRENT_RANGE_SELECT: &str = "s/i/range/select";
/// Lowest range auto-ranging may select, 0..=5.
pub const S_CURRENT_RANGE_MIN: &str = "s/i/range/min";
/// Highest range auto-ranging may select, 0..=5.
pub const S_CURRENT_RANGE_MAX: &str = "s/i/range/max";

/// Voltage range selection mode: `auto` or `manual`.
pub const S_VOLTAGE_RANGE_MODE: &str = "s/v/range/mode";
/// Voltage range to use in manual mode: `2V` or `15V`.
pub const S_VOLTAGE_RANGE_SELECT: &str = "s/v/range/select";

/// Enable on-instrument statistics.
pub const S_STATS_CTRL: &str = "s/stats/ctrl";
/// Samples per statistics block, counted at 1 Msps. 500000 gives 2 Hz.
pub const S_STATS_SCNT: &str = "s/stats/scnt";
/// Reset the running charge and energy integrals.
pub const S_STATS_CLEAR: &str = "s/stats/!clear";

/// Enable the sample id to UTC mapping stream.
pub const S_UTC_CTRL: &str = "s/utc/ctrl";

/// Device state: 0 not present, 1 closed, 2 opened.
pub const H_STATE: &str = "h/state";
/// Asynchronous error reports.
pub const H_ERROR: &str = "h/!error";
/// Periodic operational metrics.
pub const H_STATUS: &str = "h/!status";

/// Values accepted by the current range mode topic.
pub mod current_range_mode {
    /// Hardware picks the range.
    pub const AUTO: &str = "auto";
    /// Host picks the range via `s/i/range/select`.
    pub const MANUAL: &str = "manual";
    /// Range switching disabled.
    pub const OFF: &str = "off";
}

/// Values accepted by the voltage range select topic.
pub mod voltage_range {
    /// The 2 volt range.
    pub const V2: &str = "2V";
    /// The 15 volt range.
    pub const V15: &str = "15V";
}

/// Build the control topic for general purpose input `n`.
///
/// Channels 0 through 3 are the external inputs; channel 7 is the trigger
/// input. Returns `None` for any other channel.
pub fn gpi_ctrl(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/gpi/0/ctrl",
        1 => "s/gpi/1/ctrl",
        2 => "s/gpi/2/ctrl",
        3 => "s/gpi/3/ctrl",
        7 => "s/gpi/7/ctrl",
        _ => return None,
    })
}

/// Force the general purpose outputs to an exact bitmap.
///
/// Bit `n` drives output `n`, numbered as the inputs are: 0 through 3, and 7
/// for the trigger line. Published to the literal `+` topic, which is how the
/// instrument's own driver addresses these.
pub const S_GPO_VALUE: &str = "s/gpo/+/!value";

/// Drive the named general purpose outputs high, leaving the rest alone.
pub const S_GPO_SET: &str = "s/gpo/+/!set";

/// Drive the named general purpose outputs low, leaving the rest alone.
pub const S_GPO_CLEAR: &str = "s/gpo/+/!clr";

/// Ask the instrument for the present general purpose input levels.
pub const S_GPI_REQUEST: &str = "s/gpi/+/!req";

/// Whether the trigger line is an input or an output.
pub const C_TRIGGER_DIR: &str = "c/trigger/dir";

/// Downsampling filter used between 2 MHz and 1 MHz.
///
/// `0` is wideband, `1` a first-order sinc. Wideband keeps the bandwidth and
/// lets aliases through; sinc1 trades bandwidth for rejecting them.
pub const S_DOWNSAMPLE_MODE: &str = "s/dwn2/mode";

/// How the general purpose inputs are downsampled.
///
/// `0` off, `1` toggle, `2` first, `3` majority. Published to the literal
/// `+` topic, which applies it to every line at once.
pub const S_GPI_FILTER_MODE: &str = "s/gpi/+/dwnN/mode";

/// Downsample factor applied to the general purpose inputs, 0 to 2047.
pub const S_GPI_FILTER_N: &str = "s/gpi/+/dwnN/N";

/// Which line the timesync pulse-per-second is taken from.
pub const S_PPS_INPUT: &str = "s/ts/pps_in";

/// Make-before-break MOSFET counter, in current-range switch cycles.
pub const S_RANGE_MB4B: &str = "s/i/range/mb4b";

/// Compensation for the most sensitive current range.
pub const S_RANGE_S5_COMPENSATION: &str = "s/i/range/s5_cmp";

/// Whether the ADC forwards samples or a summary.
pub const S_RANGE_ADC_FORWARD: &str = "s/i/range/adc_fwd";

/// Enable software control of the controller's status LED.
pub const C_LED_ENABLE: &str = "c/led/en";

/// Blink pattern for one of the controller's LEDs.
///
/// `None` for any colour the instrument does not have.
pub fn led_controller(colour: &str) -> Option<&'static str> {
    Some(match colour {
        "red" => "c/led/red",
        "green" => "c/led/green",
        "blue" => "c/led/blue",
        _ => return None,
    })
}

/// Enable software control of the sensor's LED.
pub const S_LED_ENABLE: &str = "s/led/en";

/// Blink pattern for one of the sensor's LEDs.
pub fn led_sensor(colour: &str) -> Option<&'static str> {
    Some(match colour {
        "red" => "s/led/red",
        "green" => "s/led/green",
        "blue" => "s/led/blue",
        _ => return None,
    })
}

/// Clear every engaged fuse at once.
pub const S_FUSE_CLEAR_ALL: &str = "s/fuse/+/!clear";

/// Fuses the instrument exposes.
///
/// 0 and 1 are yours to configure. 30 and 31 are the instrument's own
/// protection, and its metadata says disabling them is at your own risk.
pub const FUSES: [u8; 4] = [0, 1, 30, 31];

/// Enable topic for fuse `n`.
pub fn fuse_enable(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/fuse/0/en",
        1 => "s/fuse/1/en",
        30 => "s/fuse/30/en",
        31 => "s/fuse/31/en",
        _ => return None,
    })
}

/// Engaged topic for fuse `n`: reads 1 when tripped, write 0 to clear it.
pub fn fuse_engaged(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/fuse/0/engaged",
        1 => "s/fuse/1/engaged",
        30 => "s/fuse/30/engaged",
        31 => "s/fuse/31/engaged",
        _ => return None,
    })
}

/// Accumulation constant for fuse `n`, in 4q14 fixed point.
pub fn fuse_accumulation(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/fuse/0/F",
        1 => "s/fuse/1/F",
        _ => return None,
    })
}

/// Dissipation constant for fuse `n`, in 8q10 fixed point.
pub fn fuse_dissipation(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/fuse/0/K",
        1 => "s/fuse/1/K",
        _ => return None,
    })
}

/// Build the control topic for raw ADC channel `n`, 0 through 3.
pub fn adc_ctrl(n: u8) -> Option<&'static str> {
    Some(match n {
        0 => "s/adc/0/ctrl",
        1 => "s/adc/1/ctrl",
        2 => "s/adc/2/ctrl",
        3 => "s/adc/3/ctrl",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::PORT_MAP;

    #[test]
    fn control_topics_agree_with_the_streaming_port_map() {
        assert_eq!(PORT_MAP[5].ctrl_topic, Some(S_CURRENT_CTRL));
        assert_eq!(PORT_MAP[6].ctrl_topic, Some(S_VOLTAGE_CTRL));
        assert_eq!(PORT_MAP[7].ctrl_topic, Some(S_POWER_CTRL));
        assert_eq!(PORT_MAP[4].ctrl_topic, Some(S_CURRENT_RANGE_CTRL));
        assert_eq!(PORT_MAP[14].ctrl_topic, Some(S_STATS_CTRL));
    }

    #[test]
    fn gpi_helper_covers_the_valid_channels_only() {
        assert_eq!(gpi_ctrl(0), Some(PORT_MAP[8].ctrl_topic.unwrap()));
        assert_eq!(gpi_ctrl(7), Some(PORT_MAP[12].ctrl_topic.unwrap()));
        assert_eq!(gpi_ctrl(4), None);
        assert_eq!(gpi_ctrl(8), None);
    }

    #[test]
    fn adc_helper_covers_four_channels() {
        for n in 0..4u8 {
            assert_eq!(adc_ctrl(n), PORT_MAP[n as usize].ctrl_topic);
        }
        assert_eq!(adc_ctrl(4), None);
    }

    #[test]
    fn every_topic_fits_the_fixed_width_topic_field() {
        let all = [
            C_FW_VERSION,
            C_HW_VERSION,
            C_MODE,
            C_POWER,
            C_COMM,
            C_GPIO_VREF,
            S_FPGA_VERSION,
            S_SAMPLE_RATE,
            S_STREAM_CTRL,
            S_CURRENT_CTRL,
            S_VOLTAGE_CTRL,
            S_POWER_CTRL,
            S_CURRENT_RANGE_CTRL,
            S_CURRENT_RANGE_MODE,
            S_CURRENT_RANGE_SELECT,
            S_CURRENT_RANGE_MIN,
            S_CURRENT_RANGE_MAX,
            S_VOLTAGE_RANGE_MODE,
            S_VOLTAGE_RANGE_SELECT,
            S_STATS_CTRL,
            S_STATS_SCNT,
            S_STATS_CLEAR,
            S_UTC_CTRL,
            H_STATE,
            H_ERROR,
            H_STATUS,
        ];
        for t in all {
            assert!(
                t.len() < crate::pubsub::TOPIC_LEN,
                "topic {t} needs {} bytes plus a NUL",
                t.len()
            );
        }
    }
}
