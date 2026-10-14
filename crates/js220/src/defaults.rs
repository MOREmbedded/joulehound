//! The known-good parameter set pushed to the instrument on open.
//!
//! Connecting does not leave the JS220 in a configuration the host can rely
//! on. The official driver restores a complete parameter set before it enables
//! any data port, and doing the same is what makes streaming start reliably:
//! publishing only the few topics a capture needs leaves the rest in whatever
//! state the previous session left them, and the stream then often fails to
//! start at all.
//!
//! This table was captured from the reference driver's own debug log while it
//! opened a JS220, so it is the sequence a known-working host actually sends,
//! in order, rather than a reading of the documentation.


use js220_proto::value::Value;

/// One parameter to restore.
pub struct Param {
    /// Topic, relative to the device.
    pub topic: &'static str,
    /// Value to publish, retained.
    pub value: Value,
}

const fn u8p(topic: &'static str, v: u8) -> Param {
    Param {
        topic,
        value: Value::U8(v),
    }
}

const fn u32p(topic: &'static str, v: u32) -> Param {
    Param {
        topic,
        value: Value::U32(v),
    }
}

const fn i32p(topic: &'static str, v: i32) -> Param {
    Param {
        topic,
        value: Value::I32(v),
    }
}

/// The full restore sequence, in the order the reference driver sends it.
///
/// Order is preserved deliberately. The instrument applies these in sequence,
/// and some settings depend on others already being in place.
pub static DEFAULTS: &[Param] = &[
    u8p("c/led/en", 0),
    u8p("c/led/red", 0),
    u8p("c/led/green", 0),
    u8p("c/led/blue", 0),
    u8p("c/s/comm/echo/msg_cnt", 0),
    u32p("c/s/comm/echo/msg_sz", 4),
    i32p("c/s/comm/echo/status", 0),
    u8p("c/trigger/dir", 0),
    u8p("c/gpio/vref", 0),
    u8p("s/led/en", 0),
    u8p("s/led/red", 0),
    u8p("s/led/green", 0),
    u8p("s/led/blue", 0),
    u8p("s/i/range/mode", 0),
    u8p("s/i/range/mb4b", 15),
    u8p("s/i/range/select", 0),
    u8p("s/i/range/adc_fwd", 0),
    u8p("s/i/range/max", 5),
    u8p("s/i/range/s5_cmp", 0),
    u8p("s/i/range/min", 0),
    u8p("s/i/range/ctrl", 0),
    u8p("s/i/ctrl", 0),
    u8p("s/dbg/mode", 0),
    u8p("s/v/range/mode", 0),
    u8p("s/v/range/select", 0),
    u8p("s/v/ctrl", 0),
    u32p("s/stats/scnt", 500_000),
    u8p("s/stats/ctrl", 0),
    u32p("s/dwn2/mode", 0),
    u32p("s/dwnN/N", 1),
    u32p("s/gpi/+/dwnN/N", 1),
    u32p("s/gpi/+/dwnN/mode", 0),
    u8p("s/gpi/0/ctrl", 0),
    u8p("s/gpi/1/ctrl", 0),
    u8p("s/gpi/2/ctrl", 0),
    u8p("s/gpi/3/ctrl", 0),
    u8p("s/gpi/7/ctrl", 0),
    u32p("s/ts/pps_in", 7),
    u32p("s/fuse/0/F", 0),
    u32p("s/fuse/0/K", 0),
    u8p("s/fuse/0/en", 0),
    u8p("s/fuse/0/engaged", 0),
    u32p("s/fuse/1/F", 0),
    u32p("s/fuse/1/K", 0),
    u8p("s/fuse/1/en", 0),
    u8p("s/fuse/1/engaged", 0),
    u8p("s/fuse/30/en", 1),
    u8p("s/fuse/30/engaged", 0),
    u8p("s/fuse/31/en", 1),
    u8p("s/fuse/31/engaged", 0),
    u8p("s/adc/0/ctrl", 0),
    u8p("s/adc/1/ctrl", 0),
    u8p("s/adc/2/ctrl", 0),
    u8p("s/adc/3/ctrl", 0),
    u8p("s/p/ctrl", 0),
    u8p("s/uart/0/ctrl", 0),
];

#[cfg(test)]
mod tests {
    use super::*;
    use js220_proto::pubsub::PubSubMsg;

    #[test]
    fn every_default_encodes_into_a_single_frame() {
        for p in DEFAULTS {
            let msg = PubSubMsg::new(p.topic, p.value.clone());
            assert!(
                msg.encode().is_some(),
                "{} does not fit in one frame",
                p.topic
            );
        }
    }

    #[test]
    fn topics_fit_the_fixed_width_field() {
        for p in DEFAULTS {
            assert!(
                p.topic.len() < js220_proto::pubsub::TOPIC_LEN,
                "{} is too long for the topic field",
                p.topic
            );
        }
    }

    #[test]
    fn no_topic_is_restored_twice() {
        let mut seen: Vec<&str> = Vec::new();
        for p in DEFAULTS {
            assert!(!seen.contains(&p.topic), "{} appears twice", p.topic);
            seen.push(p.topic);
        }
    }

    #[test]
    fn every_data_port_is_left_disabled() {
        // The restore must not start any stream; `stream_start` owns that.
        for topic in [
            "s/i/ctrl",
            "s/v/ctrl",
            "s/p/ctrl",
            "s/stats/ctrl",
            "s/uart/0/ctrl",
            "s/adc/0/ctrl",
        ] {
            let p = DEFAULTS
                .iter()
                .find(|p| p.topic == topic)
                .unwrap_or_else(|| panic!("{topic} missing from the restore set"));
            assert_eq!(p.value, Value::U8(0), "{topic} must be restored disabled");
        }
    }

    #[test]
    fn the_protective_fuses_stay_enabled() {
        // Fuses 30 and 31 are the shunt and overcurrent protection.
        for topic in ["s/fuse/30/en", "s/fuse/31/en"] {
            let p = DEFAULTS.iter().find(|p| p.topic == topic).unwrap();
            assert_eq!(p.value, Value::U8(1), "{topic} must stay enabled");
        }
    }
}
